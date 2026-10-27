pub mod mqtt {
    use crate::homie::{self, Command};
    use crate::modbus;
    use rumqttc::v5::mqttbytes::v5::LastWill;
    use rumqttc::v5::{AsyncClient, Event, EventLoop, Incoming, MqttOptions};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio::time::Instant;

    /// Start a Pylontech Force H2 to MQTT proxy which exposes a homie interface to the battery
    #[derive(clap::Parser)]
    pub struct Args {
        #[clap(flatten)]
        modbus: modbus::Args,
        #[clap(flatten)]
        device_args: homie::Args,

        /// How to connect to the MQTT broker.
        ///
        /// The value is expected to be provided as an URL, such as:
        /// `mqtt://location:1883?client_id=hostname` for plain text connection or
        /// `mqtts://location:1883?client_id=hostname` for TLS protected connection.
        #[clap(short = 'm', long)]
        mqtt_broker: String,

        /// To be provided together with `--mqtt-password` to use password based authentication
        /// with the broker.
        #[clap(short = 'u', long, requires = "mqtt_password")]
        mqtt_user: Option<String>,

        /// To be provided together with `--mqtt-user` to use password based authentication with
        /// the broker.
        #[clap(short = 'p', long, requires = "mqtt_user")]
        mqtt_password: Option<String>,

        /// The ID of the bridge device as seen in the MQTT topic and homie description.
        ///
        /// The battery is a child device of the bridge, and its ID is this with the serial number of
        /// the battery added.
        #[clap(long, default_value = "pylonforce")]
        device_id: String,
    }

    #[derive(thiserror::Error, Debug)]
    pub enum Error {
        #[error("could not progress the homie device")]
        DeviceStep(#[source] crate::homie::Error),
        #[error("specified device ID {1} is invalid")]
        InvalidDeviceId(#[source] homie5::InvalidHomieIDError, String),
        #[error("MQTT broker address {1} is not valid")]
        InvalidBrokerAddress(#[source] rumqttc::v5::OptionError, String),
        #[error("could not find the battery")]
        ResolveBattery(#[source] modbus::Error),
        #[error("communication with the battery has stopped unexpectedly")]
        ModbusTask(#[source] tokio::task::JoinError),
        #[error("communication with the battery has ended")]
        ModbusEnded,
    }

    fn commands_gone() -> Error {
        Error::DeviceStep(homie::Error::CommandsGone)
    }

    /// How long to wait before trying to reach the broker again, after it has failed this many
    /// times in a row.
    fn reconnect_delay(failures: u32) -> Duration {
        Duration::from_secs(1 << failures.saturating_sub(1).min(5)).min(Duration::from_secs(30))
    }

    /// Wait until `at`, then poll the MQTT connection. Polling is what (re)connects it.
    async fn poll_at(
        client_loop: &mut EventLoop,
        at: Instant,
    ) -> Result<Event, rumqttc::v5::ConnectionError> {
        tokio::time::sleep_until(at).await;
        client_loop.poll().await
    }

    /// The message that the broker publishes if the connection is lost.
    fn last_will(will: homie5::client::LastWill) -> LastWill {
        let qos = homie::convert_qos(will.qos);
        LastWill::new(will.topic, will.message, qos, will.retain, None)
    }

    #[tokio::main(flavor = "current_thread")]
    #[expect(clippy::result_large_err)]
    pub async fn run(args: Args) -> Result<(), Error> {
        serve(args).await
    }

    async fn serve(args: Args) -> Result<(), Error> {
        let mut mqtt_options = MqttOptions::parse_url(&args.mqtt_broker)
            .map_err(|e| Error::InvalidBrokerAddress(e, args.mqtt_broker))?;
        if let Some((u, p)) =
            args.mqtt_user.as_ref().and_then(|u| Some((u, args.mqtt_password.as_ref()?)))
        {
            mqtt_options.set_credentials(u, p);
        }

        let device_id = args
            .device_id
            .clone()
            .try_into()
            .map_err(|e| Error::InvalidDeviceId(e, args.device_id))?;
        let polls = homie::poll_plan(&args.device_args);
        let modbus::Device { handle, task: mut modbus_task } =
            modbus::connect(&args.modbus, polls).await.map_err(Error::ResolveBattery)?;

        // The will is that of the bridge, as that is the root device: the convention has it take
        // every device in the tree with it.
        let (protocol, will) =
            homie5::Homie5DeviceProtocol::new(device_id, homie5::HomieDomain::Default);
        mqtt_options.set_last_will(last_will(will));
        let (client, mut client_loop) = AsyncClient::new(mqtt_options, 100);
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let mut device =
            homie::Device::new(&args.device_args, client, protocol, handle, command_rx);
        command_tx.send(Command::Republish).map_err(|_| commands_gone())?;
        let mut failures = 0u32;
        let mut poll_not_before = Instant::now();
        loop {
            // SUBTLE: `step` is not cancellation safe -- it may be in the middle of publishing
            // the resulting changes (which can block on the MQTT request queue being full, and
            // that queue is only drained by `client_loop.poll()`.) Dropping it would lose some of
            // the publishes. So the same future is kept and polled across MQTT events until it
            // completes.
            let mut step = std::pin::pin!(device.step());
            let step_result = loop {
                let mqtt_event = tokio::select! {
                    biased;
                    result = poll_at(&mut client_loop, poll_not_before) => match result {
                        Ok(event) => event,
                        Err(error) => {
                            let error = &error as &dyn std::error::Error;
                            if failures == 0 {
                                tracing::warn!(error, "cannot reach the MQTT broker, will keep trying");
                            } else {
                                tracing::debug!(error, "could not connect to the MQTT broker");
                            }
                            failures = failures.saturating_add(1);
                            poll_not_before = Instant::now() + reconnect_delay(failures);
                            continue;
                        }
                    },
                    result = &mut modbus_task => return Err(match result {
                        Ok(()) => Error::ModbusEnded,
                        Err(e) => Error::ModbusTask(e),
                    }),
                    result = &mut step => break result,
                };
                let command = match mqtt_event {
                    // The broker has published the last will and may have lost the retained
                    // messages.
                    Event::Incoming(Incoming::ConnAck(_)) if failures > 0 => {
                        tracing::info!("connected to the MQTT broker again");
                        failures = 0;
                        Some(Command::Republish)
                    }
                    // Acknowledgements for our own publishes arrive for every single publish, and
                    // failures are reported as errors from `poll`.
                    Event::Incoming(
                        Incoming::PubAck(_)
                        | Incoming::PubRec(_)
                        | Incoming::PubRel(_)
                        | Incoming::PubComp(_)
                        | Incoming::SubAck(_)
                        | Incoming::PingResp(_),
                    )
                    | Event::Outgoing(_) => None,
                    Event::Incoming(event) => {
                        tracing::debug!(?event, "incoming mqtt event not handled");
                        None
                    }
                };
                if let Some(command) = command {
                    command_tx.send(command).map_err(|_| commands_gone())?;
                }
            };
            step_result.map_err(Error::DeviceStep)?;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::battery::tests::fixture;
        use crate::modbus::Space;
        use crate::modbus::fake::{self, Step};
        use clap::Parser as _;
        use std::net::SocketAddr;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::{TcpListener, TcpStream};

        /// (The number of the connection, topic, payload) of what the bridge has published.
        type Publishes = mpsc::UnboundedReceiver<(usize, String, String)>;

        /// A battery with the registers of the fixture. It hangs up on everything for as long as `present` is not set.
        async fn serve_battery(present: Arc<AtomicBool>) -> SocketAddr {
            let sample = fixture();
            let (address, _server) = fake::start(move |request| {
                if !present.load(SeqCst) {
                    return vec![Step::Close];
                }
                let words = (0..request.count)
                    .map(|i| sample.get(Space::Holding, request.start + i).unwrap_or(0));
                vec![Step::Send(request.data_frame_with(words))]
            })
            .await;
            address
        }

        /// The will in the `CONNECT` packet: its topic and payload.
        fn will_of(body: &[u8]) -> Option<(String, String)> {
            let flags = *body.get(7)?;
            // Protocol name, version, flags, keep alive and then the properties.
            let mut at = 10;
            let skip_properties = |at: &mut usize| -> Option<()> {
                // The properties we deal with are shorter than 128 bytes.
                *at += 1 + usize::from(*body.get(*at)?);
                Some(())
            };
            skip_properties(&mut at)?;
            let string = |at: &mut usize| -> Option<String> {
                let length =
                    usize::from(u16::from_be_bytes([*body.get(*at)?, *body.get(*at + 1)?]));
                let bytes = body.get(*at + 2..*at + 2 + length)?;
                *at += 2 + length;
                Some(String::from_utf8_lossy(bytes).into_owned())
            };
            string(&mut at)?;
            if flags & 0x04 == 0 {
                return None;
            }
            skip_properties(&mut at)?;
            let topic = string(&mut at)?;
            let payload = string(&mut at)?;
            Some((topic, payload))
        }

        /// What a fake broker lets the test do.
        struct Broker {
            publishes: Publishes,
            drop: mpsc::UnboundedSender<()>,
            /// The will in the `CONNECT` packets.
            wills: mpsc::UnboundedReceiver<(String, String)>,
        }

        /// A broker that gets along with just enough of MQTT.
        fn serve_broker(listener: TcpListener) -> Broker {
            let (publishes_tx, publishes) = mpsc::unbounded_channel();
            let (wills_tx, wills) = mpsc::unbounded_channel();
            let (drop_tx, mut drop_rx) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                let mut connection = 0;
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else { return };
                    let mut published = false;
                    loop {
                        let packet = tokio::select! {
                            packet = read_packet(&mut socket) => packet,
                            _ = drop_rx.recv() => None,
                        };
                        let Some((header, body)) = packet else { break };
                        let id = body.get(..2).unwrap_or_default();
                        let reply: Vec<u8> = match header >> 4 {
                            1 => {
                                if let Some(will) = will_of(&body) {
                                    let _ = wills_tx.send(will);
                                }
                                vec![0x20, 3, 0, 0, 0]
                            }
                            3 => {
                                let topic_len = usize::from(u16::from_be_bytes([body[0], body[1]]));
                                let topic = String::from_utf8_lossy(&body[2..2 + topic_len]);
                                let mut payload = 2 + topic_len;
                                let qos = (header >> 1) & 3;
                                if qos > 0 {
                                    payload += 2;
                                }
                                payload += 1 + usize::from(body[payload]);
                                let payload = String::from_utf8_lossy(&body[payload..]);
                                published = true;
                                let _ =
                                    publishes_tx.send((connection, topic.into(), payload.into()));
                                let id = &body[2 + topic_len..];
                                match qos {
                                    1 => vec![0x40, 2, id[0], id[1]],
                                    2 => vec![0x50, 2, id[0], id[1]],
                                    _ => continue,
                                }
                            }
                            8 => {
                                vec![0x90, 3, id[0], id[1], 0]
                            }
                            6 => vec![0x70, 2, id[0], id[1]],
                            12 => vec![0xd0, 0],
                            _ => continue,
                        };
                        if socket.write_all(&reply).await.is_err() {
                            break;
                        }
                    }
                    // The connections that have not published anything (the bridge is free to give
                    // up on one before it is done) do not count.
                    connection += usize::from(published);
                }
            });
            Broker { publishes, drop: drop_tx, wills }
        }

        async fn read_packet(socket: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
            let header = socket.read_u8().await.ok()?;
            let (mut length, mut shift) = (0, 0);
            loop {
                let byte = socket.read_u8().await.ok()?;
                length |= usize::from(byte & 0x7f) << shift;
                shift += 7;
                if byte & 0x80 == 0 {
                    break;
                }
            }
            let mut body = vec![0; length];
            socket.read_exact(&mut body).await.ok()?;
            Some((header, body))
        }

        const BRIDGE: &str = "pylonforce-test";
        const STACK: &str = "pylonforce-test-pfh2test00000001";

        /// A running bridge, a fake battery and a fake broker, for as long as the scenario goes.
        async fn scenario(present: Arc<AtomicBool>, scenario: impl AsyncFnOnce(Broker)) {
            let battery = serve_battery(present).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let broker = listener.local_addr().unwrap();
            let broker_handle = serve_broker(listener);
            let args = Args::parse_from([
                "test",
                "--tcp",
                &battery.to_string(),
                "--mqtt-broker",
                &format!("mqtt://{broker}?client_id=test"),
                "--device-id",
                BRIDGE,
                "--poll-period",
                "100ms",
                "--timeout",
                "1s",
                "--request-gap",
                "1ms",
                "--unavailable-after",
                "500ms",
            ]);
            tokio::select! {
                result = serve(args) => panic!("the bridge has ended: {result:?}"),
                result = tokio::time::timeout(Duration::from_secs(15), scenario(broker_handle)) => {
                    result.expect("the scenario did not play out");
                }
            }
        }

        /// Wait for the device to publish its `$state` over the connection.
        async fn until_state(
            publishes: &mut Publishes,
            device: &str,
            connection: usize,
            state: &str,
        ) {
            let topic = format!("homie/5/{device}/$state");
            while let Some((from, published, payload)) = publishes.recv().await {
                if (from, published.as_str(), payload.as_str()) == (connection, &topic, state) {
                    return;
                }
            }
            panic!("the broker has gone");
        }

        #[test]
        fn small_things() {
            let (_, will) = homie5::Homie5DeviceProtocol::new(
                "pylonforce".to_string().try_into().unwrap(),
                homie5::HomieDomain::Default,
            );
            let will = last_will(will);
            assert_eq!(will.topic, "homie/5/pylonforce/$state");
            assert_eq!(&will.message[..], b"lost");
            assert!(will.retain);
            assert_eq!(will.qos, rumqttc::v5::mqttbytes::QoS::ExactlyOnce);

            let delays: Vec<_> = (0..9).map(|n| reconnect_delay(n).as_secs()).collect();
            assert_eq!(delays, [1, 1, 2, 4, 8, 16, 30, 30, 30]);
        }

        #[tokio::test]
        async fn the_device_id_is_validated() {
            for id in ["Pylon Force", ""] {
                let broker = "mqtt://x?client_id=test";
                let argv = ["test", "--tcp", "x", "--mqtt-broker", broker, "--device-id", id];
                let result = serve(Args::parse_from(argv)).await;
                assert!(matches!(result, Err(Error::InvalidDeviceId(..))));
            }
        }

        #[tokio::test]
        async fn the_bridge_and_the_stack_are_published_with_the_will_of_the_bridge() {
            scenario(Arc::new(AtomicBool::new(true)), async |mut broker| {
                until_state(&mut broker.publishes, BRIDGE, 0, "ready").await;
                until_state(&mut broker.publishes, STACK, 0, "ready").await;
                let will = broker.wills.recv().await.unwrap();
                assert_eq!(will, (format!("homie/5/{BRIDGE}/$state"), "lost".to_string()));
            })
            .await;
        }

        #[tokio::test]
        async fn the_devices_are_published_again_when_the_broker_comes_back() {
            scenario(Arc::new(AtomicBool::new(true)), async |mut broker| {
                until_state(&mut broker.publishes, STACK, 0, "ready").await;
                broker.drop.send(()).unwrap();
                // The new connection starts out with nothing: no last will has been dealt with
                // and no retained messages are there.
                until_state(&mut broker.publishes, BRIDGE, 1, "ready").await;
                until_state(&mut broker.publishes, STACK, 1, "ready").await;
                assert_eq!(broker.wills.recv().await.unwrap().1, "lost");
                assert_eq!(broker.wills.recv().await.unwrap().1, "lost");
            })
            .await;
        }

        #[tokio::test]
        async fn the_stack_is_disconnected_while_the_battery_is_gone() {
            let present = Arc::new(AtomicBool::new(true));
            scenario(present.clone(), async |mut broker| {
                until_state(&mut broker.publishes, STACK, 0, "ready").await;
                present.store(false, SeqCst);
                until_state(&mut broker.publishes, STACK, 0, "disconnected").await;
                present.store(true, SeqCst);
                until_state(&mut broker.publishes, STACK, 0, "ready").await;
            })
            .await;
        }
    }
}
