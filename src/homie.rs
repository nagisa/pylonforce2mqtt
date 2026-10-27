//! The homie side of things: turns the contents of the register [`Snapshot`] into homie
//! properties and publishes them when they change.
//!
//! There are two devices. The bridge is the root: it has no nodes and exists so that the MQTT last
//! will has something to be about. Its child, the stack, is the battery itself. Its ID has the
//! serial number of the battery in it, so it exists once the serial number has been read. Only the
//! stack is ever in a state other than `ready`.
//!
//! [`Snapshot`]: crate::modbus::Snapshot

mod nodes;

use crate::battery::{self, Reading, Topology};
use crate::homie::nodes::{Layout, Property};
use crate::modbus::{self, PollBlock};
use homie5::client::{Publish, QoS};
use homie5::device_description::DeviceDescriptionBuilder;
use homie5::{Homie5DeviceProtocol, HomieDeviceStatus, HomieID};
use tokio::sync::mpsc;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("could not construct the device description")]
    BuildDeviceDescription(#[source] homie5::Homie5ProtocolError),
    #[error("could not {1}")]
    Mqtt(#[source] rumqttc::v5::ClientError, String),
    #[error("nothing is going to ask the device to publish anything anymore")]
    CommandsGone,
}

fn parse_poll_period(value: &str) -> Result<humantime::Duration, String> {
    let period: humantime::Duration = value.parse().map_err(|e| format!("{e}"))?;
    if period.is_zero() { Err("the period must not be zero".to_string()) } else { Ok(period) }
}

#[derive(clap::Parser, Clone)]
#[group(id = "homie::Args")]
pub struct Args {
    /// How often the battery is asked for the state it is in.
    ///
    /// The identification of the battery is read much less often, as it does not change.
    #[clap(long, short = 'r', default_value = "5s", value_parser = parse_poll_period)]
    poll_period: humantime::Duration,

    /// Also publish the voltage of every cell.
    ///
    /// This makes for a node for each of the cells (there may be up to 150 of them), and the
    /// battery is asked for the voltages of all of them every `--poll-period`.
    #[clap(long)]
    detailed: bool,
}

pub(crate) fn poll_plan(args: &Args) -> Vec<PollBlock> {
    battery::poll_plan(*args.poll_period, args.detailed)
}

/// The ID of the device for the stack: the ID of the bridge and the serial number of the battery
/// in lowercase, with a hyphen for every run of anything but letters and digits. `None` if there
/// is nothing left of the serial number.
fn stack_id(bridge: &HomieID, serial: &str) -> Option<HomieID> {
    let serial = serial.to_lowercase();
    let parts: Vec<_> = serial.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    let suffix = parts.into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("-");
    if suffix.is_empty() { None } else { HomieID::try_from(format!("{bridge}-{suffix}")).ok() }
}

/// The value to publish for the reading, if there is one. Tracks whether the last reading was
/// invalid, to log once per streak rather than for every poll.
fn judge(property: &Property, invalid: &mut bool, reading: &Reading) -> Option<String> {
    let (node_id, prop_id) = (&property.node, &property.id);
    match reading {
        None => None,
        Some(Ok(value)) => {
            if std::mem::take(invalid) {
                tracing::info!(%node_id, %prop_id, "the value is valid again");
            }
            Some(value.clone())
        }
        Some(Err(reason)) => {
            if !std::mem::replace(invalid, true) {
                let message = "the battery has reported an invalid value, the last valid one stays";
                tracing::warn!(%node_id, %prop_id, %reason, "{message}");
            }
            None
        }
    }
}

/// What the stack was described by: the name and the topology.
type DescribedBy = (String, Option<Topology>);

struct Stack {
    protocol: Homie5DeviceProtocol,
    /// The state as it is published. `None` before anything is.
    state: Option<HomieDeviceStatus>,
    /// The last topology that made sense. The stack is described by it if the battery makes no
    /// sense at the moment.
    topology: Option<Topology>,
    described_by: Option<DescribedBy>,
    /// `None` until the battery can be described.
    layout: Option<Layout>,
    /// What we have last published and whether the last reading was not valid, both indexed like
    /// the properties of the `layout`.
    published: Vec<Option<String>>,
    invalid: Vec<bool>,
}

impl Stack {
    fn new(protocol: Homie5DeviceProtocol) -> Self {
        Stack {
            protocol,
            state: None,
            topology: None,
            described_by: None,
            layout: None,
            published: Vec::new(),
            invalid: Vec::new(),
        }
    }

    fn target_state(&self, available: bool) -> HomieDeviceStatus {
        match &self.layout {
            Some(_) if available => HomieDeviceStatus::Ready,
            Some(_) => HomieDeviceStatus::Disconnected,
            None => HomieDeviceStatus::Init,
        }
    }
}

/// What the registers say right now, worked out in one go for the registers to be consistent.
struct Observed {
    available: bool,
    /// How the stack is to be described, if that is different from what it is described by.
    layout: Option<(Layout, DescribedBy)>,
    /// The readings of the properties of the layout that is to be in effect.
    readings: Vec<Reading>,
}

pub(crate) struct Device {
    detailed: bool,
    mqtt: rumqttc::v5::AsyncClient,
    bridge: Homie5DeviceProtocol,
    modbus: modbus::Handle,
    commands: mpsc::UnboundedReceiver<Command>,
    /// Whether anything is published (to the broker that we are connected to) yet.
    published: bool,
    /// Once the battery has said what its serial number is.
    stack: Option<Stack>,
}

impl Device {
    pub(crate) fn new(
        args: &Args,
        mqtt: rumqttc::v5::AsyncClient,
        bridge: Homie5DeviceProtocol,
        modbus: modbus::Handle,
        commands: mpsc::UnboundedReceiver<Command>,
    ) -> Self {
        Self {
            detailed: args.detailed,
            mqtt,
            bridge,
            modbus,
            commands,
            published: false,
            stack: None,
        }
    }

    async fn publish(&self, message: Publish) -> Result<(), Error> {
        let topic = message.topic.clone();
        let (qos, retain) = (convert_qos(message.qos), message.retain);
        let published = self.mqtt.publish(message.topic, qos, retain, message.payload).await;
        published.map_err(|e| Error::Mqtt(e, format!("publish {topic}")))
    }

    async fn publish_stack_state(&mut self, state: HomieDeviceStatus) -> Result<(), Error> {
        let Some(stack) = &mut self.stack else { return Ok(()) };
        stack.state = Some(state);
        let message = stack.protocol.publish_state(state);
        self.publish(message).await
    }

    /// Publish the bridge, with the stack as its child if there is one.
    async fn publish_bridge(&self) -> Result<(), Error> {
        let children = self.stack.iter().map(|stack| stack.protocol.id().clone());
        let description = DeviceDescriptionBuilder::new().name("pylonforce2mqtt");
        let description = children.fold(description, |d, child| d.add_child(child));
        self.publish(self.bridge.publish_state(HomieDeviceStatus::Init)).await?;
        let description = self.bridge.publish_description(&description.build());
        self.publish(description.map_err(Error::BuildDeviceDescription)?).await?;
        self.publish(self.bridge.publish_state(HomieDeviceStatus::Ready)).await
    }

    /// The ID of the stack for the battery, if it has said what its serial number is.
    fn battery_stack_id(&self) -> Option<HomieID> {
        let Some(Ok(serial)) = battery::serial_number(&self.modbus.snapshot()) else { return None };
        stack_id(self.bridge.id(), &serial)
    }

    fn observe(&mut self) -> Observed {
        let snapshot = self.modbus.snapshot();
        let available = snapshot.available();
        let mut layout = None;
        let detailed = self.detailed;
        let Some(stack) = &mut self.stack else {
            return Observed { available, layout, readings: Vec::new() };
        };
        if battery::is_complete(&snapshot, detailed) {
            if let Some(Ok(topology)) = Topology::read(&snapshot) {
                stack.topology = Some(topology);
            }
            let name = battery::name(&snapshot).unwrap_or_else(|| nodes::FALLBACK_NAME.to_string());
            let described_by = (name, stack.topology);
            // Making the description is not free, so it is only done if it is to change.
            if stack.described_by.as_ref() != Some(&described_by) {
                let (name, topology) = &described_by;
                let wanted = nodes::layout(name, *topology, detailed, self.bridge.id());
                layout = Some((wanted, described_by));
            }
        }
        let current = layout.as_ref().map(|(layout, _)| layout).or(stack.layout.as_ref());
        let readings = current
            .map(|layout| layout.properties.iter().map(|p| (p.read)(&snapshot)).collect())
            .unwrap_or_default();
        Observed { available, layout, readings }
    }

    /// Bring what is published in line with the registers. With `republish`, everything is
    /// published again, as is needed when the broker has lost track of us. Otherwise only what has
    /// changed is.
    async fn update(&mut self, republish: bool) -> Result<(), Error> {
        if !republish && !self.published {
            return Ok(());
        }
        if republish {
            self.published = true;
        }
        let current = self.stack.as_ref().map(|stack| stack.protocol.id());
        let other = self.battery_stack_id().filter(|id| Some(id) != current);
        let switched = other.is_some();
        if let Some(id) = other {
            tracing::info!(device_id = %id, "the serial number of the battery is known");
            self.stack = Some(Stack::new(self.bridge.clone_for_child(id)));
        }
        if republish || switched {
            // The children first, and `init` is where the stack is going to stay until the
            // battery is there to describe it by.
            self.publish_stack_state(HomieDeviceStatus::Init).await?;
            self.publish_bridge().await?;
        }
        let mut full = republish || switched;
        if self.stack.is_none() {
            return Ok(());
        }
        let Observed { available, layout, readings } = self.observe();
        if let Some((layout, described_by)) = layout {
            let Some(stack) = &mut self.stack else { return Ok(()) };
            tracing::info!(
                name = ?layout.description.name,
                modules = stack.topology.map(|t| t.modules),
                properties = layout.properties.len(),
                "describing the stack"
            );
            stack.published = vec![None; layout.properties.len()];
            stack.invalid = vec![false; layout.properties.len()];
            stack.layout = Some(layout);
            stack.described_by = Some(described_by);
            full = true;
        } else if self.stack.as_ref().is_some_and(|stack| stack.layout.is_none()) {
            return Ok(());
        }
        if full {
            self.publish_stack(&readings, available).await
        } else {
            self.publish_changes(&readings, available).await
        }
    }

    /// Publish all of the stack. The description is only ever changed while the device is `init`.
    async fn publish_stack(&mut self, readings: &[Reading], available: bool) -> Result<(), Error> {
        if self.stack.as_ref().is_some_and(|stack| stack.state != Some(HomieDeviceStatus::Init)) {
            self.publish_stack_state(HomieDeviceStatus::Init).await?;
        }
        let Some(stack) = &self.stack else { return Ok(()) };
        let Some(layout) = &stack.layout else { return Ok(()) };
        let description = stack.protocol.publish_description(&layout.description);
        self.publish(description.map_err(Error::BuildDeviceDescription)?).await?;
        self.publish_values(readings, true).await?;
        let Some(stack) = &self.stack else { return Ok(()) };
        self.publish_stack_state(stack.target_state(available)).await
    }

    async fn publish_changes(
        &mut self,
        readings: &[Reading],
        available: bool,
    ) -> Result<(), Error> {
        self.publish_values(readings, false).await?;
        // After the values: a stack that is back is `ready` with all of the values being current.
        let Some(stack) = &self.stack else { return Ok(()) };
        let state = stack.target_state(available);
        if stack.state != Some(state) {
            self.publish_stack_state(state).await?;
            tracing::info!(%state, "the state of the stack has changed");
        }
        Ok(())
    }

    /// Publish the values of the properties that are different from the ones published (or all of
    /// them, with `force`.) A value that is not valid is not published: the last valid one stays,
    /// and is what is published when everything is published again.
    async fn publish_values(&mut self, readings: &[Reading], force: bool) -> Result<(), Error> {
        let Some(stack) = &mut self.stack else { return Ok(()) };
        let Some(layout) = &stack.layout else { return Ok(()) };
        let mut messages = Vec::new();
        let properties = layout.properties.iter().zip(readings);
        for (idx, ((property, reading), invalid)) in
            properties.zip(stack.invalid.iter_mut()).enumerate()
        {
            let published = stack.published.get(idx).and_then(|p| p.as_ref());
            let valid = judge(property, invalid, reading);
            let Some(value) = valid.or_else(|| published.cloned()) else { continue };
            if force || published != Some(&value) {
                let message =
                    stack.protocol.publish_value(&property.node, &property.id, &value, true);
                messages.push((idx, message, value));
            }
        }
        for (idx, message, value) in messages {
            self.publish(message).await?;
            let published = self.stack.as_mut().and_then(|stack| stack.published.get_mut(idx));
            if let Some(published) = published {
                *published = Some(value);
            }
        }
        Ok(())
    }

    /// Wait for something to happen and deal with it.
    ///
    /// Not cancellation safe: dropping the future in the middle of handling an event can lose
    /// some of the publishes.
    pub(crate) async fn step(&mut self) -> Result<(), Error> {
        tokio::select! {
            () = self.modbus.changed() => self.update(false).await,
            command = self.commands.recv() => match command {
                Some(Command::Republish) => self.update(true).await,
                None => Err(Error::CommandsGone),
            },
        }
    }
}

pub fn convert_qos(homie: QoS) -> rumqttc::v5::mqttbytes::QoS {
    match homie {
        QoS::AtMostOnce => rumqttc::v5::mqttbytes::QoS::AtMostOnce,
        QoS::AtLeastOnce => rumqttc::v5::mqttbytes::QoS::AtLeastOnce,
        QoS::ExactlyOnce => rumqttc::v5::mqttbytes::QoS::ExactlyOnce,
    }
}

#[derive(Debug)]
pub(crate) enum Command {
    Republish,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battery::tests::fixture;
    use crate::modbus::{Handle, Snapshot, Space};
    use clap::Parser as _;
    use rumqttc::v5::Request;
    use serde_json::{Value, json};

    macro_rules! bridge {
        ($path:literal) => {
            concat!("homie/5/pylonforce", $path)
        };
    }
    /// The serial number of the battery of the fixture is `PFH2TEST00000001`.
    macro_rules! stack {
        ($path:literal) => {
            concat!("homie/5/pylonforce-pfh2test00000001", $path)
        };
    }

    /// A device that is connected to nothing: what it publishes is queued for the test to see.
    struct Rig {
        device: Device,
        requests: flume::Receiver<Request>,
        commands: mpsc::UnboundedSender<Command>,
    }

    struct Msgs(Vec<(String, String)>);

    impl Msgs {
        fn of(&self, topic: &str) -> Vec<&str> {
            self.0.iter().filter(|(t, _)| t == topic).map(|(_, p)| p.as_str()).collect()
        }

        fn position(&self, topic: &str, payload: &str) -> usize {
            let found = self.0.iter().position(|(t, p)| t == topic && p == payload);
            found.unwrap_or_else(|| panic!("{topic} = {payload:?} not published: {:?}", self.0))
        }

        /// The last payload published to the topic.
        fn value(&self, topic: &str) -> &str {
            self.of(topic).last().unwrap_or_else(|| panic!("{topic} not published"))
        }

        fn pairs(&self) -> Vec<(&str, &str)> {
            self.0.iter().map(|(t, p)| (t.as_str(), p.as_str())).collect()
        }

        fn deleted(&self) -> Vec<&str> {
            self.0.iter().filter(|(_, p)| p.is_empty()).map(|(t, _)| t.as_str()).collect()
        }

        fn description(&self, device: &str) -> Value {
            serde_json::from_str(self.of(&format!("{device}/$description"))[0]).unwrap()
        }
    }

    fn publishes(requests: &[Request]) -> Msgs {
        let publishes = requests.iter().filter_map(|request| match request {
            Request::Publish(p) => Some((
                String::from_utf8(p.topic.to_vec()).unwrap(),
                String::from_utf8(p.payload.to_vec()).unwrap(),
            )),
            _ => None,
        });
        Msgs(publishes.collect())
    }

    fn keys(value: &Value) -> Vec<&str> {
        let mut keys: Vec<_> = value.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        keys
    }

    fn args(extra: &[&str]) -> Args {
        Args::parse_from(["test"].into_iter().chain(extra.iter().copied()))
    }

    impl Rig {
        fn new(extra: &[&str], snapshot: Snapshot) -> Rig {
            let (requests_tx, requests) = flume::unbounded();
            let client = rumqttc::v5::AsyncClient::from_senders(requests_tx);
            let id = HomieID::new_const("pylonforce");
            let (protocol, _) = Homie5DeviceProtocol::new(id, homie5::HomieDomain::Default);
            let (commands, commands_rx) = mpsc::unbounded_channel();
            let handle = Handle::detached(snapshot);
            let device = Device::new(&args(extra), client, protocol, handle, commands_rx);
            Rig { device, requests, commands }
        }

        /// The battery is there, and everything has been read.
        fn ready(extra: &[&str]) -> Rig {
            let rig = Rig::new(extra, fixture());
            rig.available(true);
            rig
        }

        /// The battery has said nothing yet.
        fn empty() -> Rig {
            Rig::new(&[], Snapshot::new())
        }

        fn set(&self, address: u16, word: u16) {
            self.device.modbus.snapshot_mut().set(Space::Holding, address, word);
        }

        fn available(&self, available: bool) {
            self.device.modbus.snapshot_mut().set_available(available);
        }

        /// Have the registers read as in the fixture.
        fn read_sample(&self, addresses: std::ops::Range<u16>) {
            let sample = fixture();
            for address in addresses {
                self.set(address, sample.get(Space::Holding, address).unwrap());
            }
        }

        fn set_serial(&self, serial: &str) {
            let mut bytes = serial.as_bytes().to_vec();
            bytes.resize(32, 0);
            for (i, word) in bytes.chunks(2).enumerate() {
                self.set(0x1450 + i as u16, u16::from_be_bytes([word[0], word[1]]));
            }
        }

        async fn update(&mut self) {
            self.device.update(false).await.unwrap();
        }

        async fn republish(&mut self) {
            self.device.update(true).await.unwrap();
        }

        /// What the MQTT client was asked to do since the last time.
        fn requests(&self) -> Vec<Request> {
            self.requests.try_iter().collect()
        }

        fn published(&self) -> Msgs {
            publishes(&self.requests())
        }

        fn stack(&self) -> &Stack {
            self.device.stack.as_ref().unwrap()
        }
    }

    #[test]
    fn the_id_of_the_stack_is_made_of_the_serial_number() {
        let bridge = HomieID::new_const("pylonforce");
        for (serial, id) in [
            ("PFH2TEST00000001", Some("pylonforce-pfh2test00000001")),
            // A hyphen for every run of anything else, and not at either end.
            ("AB 12/_3", Some("pylonforce-ab-12-3")),
            ("  --Ab--  ", Some("pylonforce-ab")),
            ("a.b.c.", Some("pylonforce-a-b-c")),
            ("", None),
            (" -_. ", None),
            ("ÄÖ", None),
        ] {
            assert_eq!(stack_id(&bridge, serial).as_ref().map(HomieID::as_str), id, "{serial}");
        }
    }

    #[tokio::test]
    async fn the_devices_are_published_children_first() {
        let mut rig = Rig::ready(&[]);
        rig.republish().await;
        let requests = rig.requests();
        let msgs = publishes(&requests);

        let bridge = msgs.description(bridge!(""));
        assert_eq!(bridge["children"], json!(["pylonforce-pfh2test00000001"]));
        assert_eq!(bridge["name"], "pylonforce2mqtt");
        assert!(bridge.get("nodes").is_none() && bridge.get("root").is_none());
        assert_eq!(msgs.of(bridge!("/$state")), ["init", "ready"]);

        let stack = msgs.description(stack!(""));
        assert_eq!(
            (&stack["root"], &stack["name"]),
            (&json!("pylonforce"), &json!("Pylon Force_H2"))
        );
        assert!(stack.get("children").is_none());
        let nodes = &stack["nodes"];
        let ids =
            ["bms", "cells", "energy", "health", "limits", "module-1", "module-2", "module-3"];
        assert_eq!(keys(nodes), [&ids[..], &["module-4", "stack"]].concat());
        assert_eq!(nodes["module-2"]["name"], "Module 2");
        assert_eq!(nodes["stack"]["properties"]["state-of-charge"]["format"], "0:100");
        assert_eq!(nodes["stack"]["properties"]["voltage"]["unit"], "V");
        assert_eq!(
            nodes["stack"]["properties"]["state"]["format"],
            "idle,charging,discharging,sleep"
        );
        assert_eq!(nodes["health"]["properties"]["alarms"]["datatype"], "json");
        let mut described = 0;
        for node in keys(nodes) {
            for property in keys(&nodes[node]["properties"]) {
                described += 1;
                msgs.value(&format!("{}/{node}/{property}", stack!("")));
            }
        }
        assert_eq!(described, 12 + 8 + 8 + 4 + 3 + 3 + 4 * 3);
        assert_eq!(msgs.value(stack!("/stack/voltage")), "383.3");

        let stack_init = msgs.position(stack!("/$state"), "init");
        let bridge_init = msgs.position(bridge!("/$state"), "init");
        let stack_ready = msgs.position(stack!("/$state"), "ready");
        let described = msgs.0.iter().position(|(t, _)| t == stack!("/$description")).unwrap();
        assert!(stack_init < bridge_init && bridge_init < stack_ready);
        assert!(stack_init < described && described < stack_ready);
        assert_eq!(msgs.of(stack!("/$state")), ["init", "ready"]);

        assert!(!requests.iter().any(|r| matches!(r, Request::Subscribe(_))));
        rig.update().await;
        assert!(rig.requests().is_empty());
        rig.republish().await;
        let again = rig.requests();
        assert_eq!(publishes(&again).pairs(), msgs.pairs());

        let mut rig = Rig::empty();
        rig.republish().await;
        let requests = rig.requests();
        let msgs = publishes(&requests);
        assert_eq!(msgs.of(bridge!("/$state")), ["init", "ready"]);
        assert!(msgs.description(bridge!(""))["children"].is_null());
        assert!(!msgs.0.iter().any(|(t, _)| t.contains("pylonforce-")));
    }

    #[tokio::test]
    async fn the_cells_are_only_there_when_asked_for() {
        let blocks = |extra: &[&str]| {
            poll_plan(&args(extra)).iter().map(|b| (b.start, b.count)).collect::<Vec<_>>()
        };
        let mut expected = vec![(0x1000, 12), (0x1450, 16), (0x1400, 58), (0x1460, 6), (0x14B0, 6)];
        assert_eq!(blocks(&[]), expected);
        expected.push((0x1500, 150));
        assert_eq!(blocks(&["--detailed"]), expected);
        let periods: Vec<_> =
            poll_plan(&args(&["--poll-period", "2s"])).iter().map(|b| b.period.as_secs()).collect();
        assert_eq!(periods, [600, 600, 2, 2, 2]);
        assert!(Args::try_parse_from(["test", "--poll-period", "0s"]).is_err());

        let mut plain = Rig::ready(&[]);
        plain.republish().await;
        let nodes = plain.published().description(stack!(""))["nodes"].clone();
        assert!(!keys(&nodes).iter().any(|node| node.contains("-cell-")));
        assert!(nodes["cells"]["properties"].get("max-cell-voltage").is_none());
        assert!(nodes["module-2"]["properties"].get("max-cell-voltage").is_none());

        let mut rig = Rig::ready(&["--detailed"]);
        rig.republish().await;
        let msgs = rig.published();
        let description = msgs.description(stack!(""));
        let nodes = &description["nodes"];
        assert_eq!(keys(nodes).len(), 6 + 4 + 120);
        assert_eq!(nodes["module-2-cell-27"]["name"], "Module 2 cell 27");
        assert_eq!(nodes["module-2-cell-27"]["properties"]["voltage"]["unit"], "V");
        assert!(nodes.get("module-2-cell-31").is_none() && nodes.get("module-5-cell-1").is_none());
        assert_eq!(
            keys(&nodes["module-2"]["properties"]),
            [
                "average-cell-voltage",
                "cell-voltage-spread",
                "max-cell-voltage",
                "max-cell-voltage-cell",
                "min-cell-voltage",
                "min-cell-voltage-cell",
                "temperature",
                "voltage"
            ]
        );
        assert_eq!(msgs.value(stack!("/module-2-cell-27/voltage")), "3.191");
        assert_eq!(msgs.value(stack!("/module-1/max-cell-voltage-cell")), "5");

        // A cell without a voltage: nothing is known of the module, and the last values stay.
        rig.set(0x1500 + 31, 0);
        rig.update().await;
        assert!(rig.requests().is_empty());
    }

    #[tokio::test]
    async fn values_are_published_only_when_they_change() {
        let mut rig = Rig::ready(&[]);
        rig.republish().await;
        rig.requests();

        // The same registers again, and one that nothing is made of.
        rig.update().await;
        rig.set(0x1417, 5);
        rig.update().await;
        assert!(rig.requests().is_empty());

        // The voltage changes, and with it the power: -0.12 A at 383.4 V.
        rig.set(0x1403, 0x0EFA);
        rig.set(0x1405, 0xFFF4);
        rig.update().await;
        let published = rig.published();
        let expected = [(stack!("/stack/voltage"), "383.4"), (stack!("/stack/current"), "-0.12")];
        assert_eq!(published.pairs(), [expected[0], expected[1], (stack!("/stack/power"), "-46")]);
        assert_eq!(rig.stack().state, Some(HomieDeviceStatus::Ready));
    }

    #[tokio::test]
    async fn invalid_values_are_not_published_and_the_last_valid_one_stays() {
        let mut rig = Rig::ready(&[]);
        rig.republish().await;
        rig.requests();
        let invalid = |rig: &Rig| rig.stack().invalid.iter().filter(|invalid| **invalid).count();

        // 101 %, and a flag that is not a flag. A different value, in the same update, is.
        rig.set(0x1407, 101);
        rig.set(0x142F, 7);
        rig.set(0x1408, 406);
        rig.update().await;
        assert_eq!(rig.published().pairs(), [(stack!("/stack/cycles"), "406")]);
        assert_eq!(rig.stack().state, Some(HomieDeviceStatus::Ready));
        assert_eq!(invalid(&rig), 2);

        rig.set(0x1407, 102);
        rig.update().await;
        assert!(rig.requests().is_empty());

        rig.republish().await;
        let msgs = rig.published();
        assert_eq!(msgs.value(stack!("/stack/state-of-charge")), "18");
        assert_eq!(msgs.value(stack!("/limits/force-charge-request")), "false");
        assert_eq!(msgs.value(stack!("/stack/cycles")), "406");

        rig.set(0x1407, 17);
        rig.update().await;
        assert_eq!(rig.published().pairs(), [(stack!("/stack/state-of-charge"), "17")]);
        assert_eq!(invalid(&rig), 1);
    }

    #[tokio::test]
    async fn the_stack_is_init_until_it_can_be_described() {
        let mut rig = Rig::empty();
        rig.update().await;
        assert!(rig.requests().is_empty());
        rig.republish().await;
        assert!(rig.published().of(stack!("/$state")).is_empty());

        for word in [0, 0x2020, 0x0102, 0x2d2d] {
            (0x1450..0x1460).for_each(|address| rig.set(address, word));
            rig.available(true);
            rig.update().await;
            assert!(rig.requests().is_empty() && rig.device.stack.is_none());
        }

        rig.read_sample(0x1450..0x1460);
        rig.update().await;
        let msgs = rig.published();
        assert_eq!(msgs.of(stack!("/$state")), ["init"]);
        assert_eq!(msgs.of(bridge!("/$state")), ["init", "ready"]);
        assert_eq!(
            msgs.description(bridge!(""))["children"],
            json!(["pylonforce-pfh2test00000001"])
        );
        assert!(msgs.of(stack!("/$description")).is_empty());

        rig.read_sample(0x1400..0x143A);
        rig.update().await;
        assert!(rig.requests().is_empty());

        *rig.device.modbus.snapshot_mut() = fixture();
        rig.available(true);
        rig.update().await;
        let msgs = rig.published();
        assert_eq!(msgs.of(stack!("/$state")), ["ready"]);
        assert_eq!(msgs.value(stack!("/stack/voltage")), "383.3");
        assert!(msgs.of(bridge!("/$state")).is_empty());
    }

    #[tokio::test]
    async fn the_stack_follows_the_availability_of_the_battery() {
        let mut rig = Rig::ready(&[]);
        rig.republish().await;
        rig.requests();

        rig.available(false);
        rig.update().await;
        assert_eq!(rig.published().pairs(), [(stack!("/$state"), "disconnected")]);
        rig.update().await;
        assert!(rig.requests().is_empty());

        rig.set(0x1407, 19);
        rig.available(true);
        rig.update().await;
        let expected = [(stack!("/stack/state-of-charge"), "19"), (stack!("/$state"), "ready")];
        assert_eq!(rig.published().pairs(), expected);
        rig.available(false);
        rig.update().await;
        rig.available(true);
        rig.update().await;
        let expected = [(stack!("/$state"), "disconnected"), (stack!("/$state"), "ready")];
        assert_eq!(rig.published().pairs(), expected);

        rig.available(false);
        rig.republish().await;
        let msgs = rig.published();
        assert_eq!(msgs.of(stack!("/$state")), ["init", "disconnected"]);
        assert_eq!(msgs.value(stack!("/stack/voltage")), "383.3");

        let mut rig = Rig::new(&[], fixture());
        rig.commands.send(Command::Republish).unwrap();
        rig.device.step().await.unwrap();
        assert_eq!(rig.published().of(stack!("/$state")), ["init", "disconnected"]);
        drop(rig.commands);
        assert!(matches!(rig.device.step().await, Err(Error::CommandsGone)));
    }

    #[tokio::test]
    async fn a_different_topology_is_described_again_through_init() {
        let mut rig = Rig::ready(&["--detailed"]);
        rig.republish().await;
        rig.requests();

        // Three modules of 30 cells.
        rig.set(0x1436, 3);
        rig.set(0x1437, 90);
        rig.update().await;
        let msgs = rig.published();
        let init = msgs.position(stack!("/$state"), "init");
        let described = msgs.0.iter().position(|(t, _)| t == stack!("/$description")).unwrap();
        let value = msgs.0.iter().position(|(t, _)| t == stack!("/module-3/voltage")).unwrap();
        let ready = msgs.position(stack!("/$state"), "ready");
        assert!(init < described && described < value && value < ready);
        assert_eq!(msgs.of(stack!("/$state")), ["init", "ready"]);
        let nodes = &msgs.description(stack!(""))["nodes"];
        assert!(nodes.get("module-3").is_some() && nodes.get("module-4").is_none());
        assert!(msgs.deleted().is_empty());
        assert_eq!(msgs.value(stack!("/cells/average-voltage")), "3.1948");

        rig.update().await;
        assert!(rig.requests().is_empty());

        // Five modules of 30 cells.
        rig.set(0x1436, 5);
        rig.set(0x1437, 150);
        rig.update().await;
        let msgs = rig.published();
        assert_eq!(msgs.of(stack!("/$state")), ["init", "ready"]);
        assert_eq!(msgs.value(stack!("/cells/max-voltage-cell")), "27");
        assert!(msgs.description(stack!(""))["nodes"].get("module-5").is_some());
    }

    #[tokio::test]
    async fn a_topology_that_makes_no_sense_is_not_used() {
        let mut rig = Rig::ready(&[]);
        rig.republish().await;
        rig.requests();
        for (modules, cells) in [(0, 120), (4, 122), (7, 140), (4, 151), (4, 0), (0xFFFF, 0xFFFF)] {
            rig.set(0x1436, modules);
            rig.set(0x1437, cells);
            rig.update().await;
            assert!(rig.requests().is_empty(), "{modules} {cells}");
        }

        let mut snapshot = fixture();
        snapshot.set(Space::Holding, 0x1437, 122);
        let mut rig = Rig::new(&["--detailed"], snapshot);
        rig.available(true);
        rig.republish().await;
        let msgs = rig.published();
        let description = msgs.description(stack!(""));
        assert_eq!(
            keys(&description["nodes"]),
            ["bms", "cells", "energy", "health", "limits", "stack"]
        );
        assert_eq!(
            keys(&description["nodes"]["cells"]["properties"]),
            ["max-voltage", "min-voltage", "voltage-spread"]
        );
        assert_eq!(msgs.of(stack!("/$state")), ["init", "ready"]);
        assert_eq!(msgs.value(stack!("/cells/max-voltage")), "3.202");
        assert!(msgs.of(stack!("/cells/average-voltage")).is_empty());

        rig.set(0x1437, 120);
        rig.update().await;
        let msgs = rig.published();
        assert_eq!(msgs.of(stack!("/$state")), ["init", "ready"]);
        assert_eq!(msgs.value(stack!("/cells/average-voltage")), "3.1943");
        assert_eq!(msgs.value(stack!("/module-4-cell-30/voltage")), "3.194");
    }

    #[tokio::test]
    async fn the_name_is_what_the_battery_calls_itself() {
        let mut rig = Rig::ready(&[]);
        rig.republish().await;
        let name = |msgs: &Msgs| msgs.description(stack!(""))["name"].clone();
        assert_eq!(name(&rig.published()), "Pylon Force_H2");

        // Not text any more, and so the name is not a good one.
        rig.set(0x1005, 0x0102);
        rig.update().await;
        let msgs = rig.published();
        assert_eq!(name(&msgs), "Pylontech Force H2");
        assert_eq!(msgs.of(stack!("/$state")), ["init", "ready"]);
        rig.set(0x1005, 0x466f);
        rig.update().await;
        assert_eq!(name(&rig.published()), "Pylon Force_H2");

        rig.set(0x1452, 0x0100);
        rig.update().await;
        assert!(rig.requests().is_empty());
        assert_eq!(rig.stack().protocol.id().as_str(), "pylonforce-pfh2test00000001");
    }

    #[tokio::test]
    async fn another_battery_makes_another_stack() {
        let mut rig = Rig::ready(&[]);
        rig.republish().await;
        rig.requests();

        const NEW: &str = "homie/5/pylonforce-z-9";
        rig.set_serial("Z 9");
        rig.update().await;
        let msgs = rig.published();
        let new_init = msgs.position(&format!("{NEW}/$state"), "init");
        let bridge_init = msgs.position(bridge!("/$state"), "init");
        let bridge_ready = msgs.position(bridge!("/$state"), "ready");
        let new_ready = msgs.position(&format!("{NEW}/$state"), "ready");
        assert!(new_init < bridge_init && bridge_init < bridge_ready && bridge_ready < new_ready);
        assert_eq!(msgs.of(bridge!("/$description")).len(), 1);
        assert_eq!(msgs.description(bridge!(""))["children"], json!(["pylonforce-z-9"]));
        assert_eq!(msgs.of(&format!("{NEW}/stack/voltage")), ["383.3"]);
        assert_eq!(msgs.description(NEW)["root"], "pylonforce");
        assert!(msgs.deleted().is_empty());
        assert!(msgs.of(stack!("/$state")).is_empty());
        assert_eq!(rig.stack().protocol.id().as_str(), "pylonforce-z-9");

        rig.set_serial("PFH2TEST00000001");
        rig.update().await;
        let msgs = rig.published();
        assert_eq!(
            msgs.description(bridge!(""))["children"],
            json!(["pylonforce-pfh2test00000001"])
        );
        assert_eq!(msgs.of(stack!("/$state")), ["init", "ready"]);
        assert!(msgs.deleted().is_empty());
    }
}
