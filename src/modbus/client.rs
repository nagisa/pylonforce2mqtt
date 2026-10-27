//! A Modbus client for a single battery on a single RS485 bus: one connection, one request at a
//! time, dropped whenever there is any doubt about what is on it. Retry pacing is up to the caller.

use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tracing::debug;

/// The most registers that the protocol lets a single response carry.
pub const MAX_COUNT: u16 = 125;

/// How long the line has to be quiet for a late response to be considered not coming anymore.
const QUIET: Duration = Duration::from_millis(100);

/// The longest to wait for the line to become quiet before giving up on the connection.
const MAX_DRAIN: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Space {
    /// Function code 3.
    Holding,
    /// Function code 4.
    Input,
}

impl Space {
    const fn function(self) -> u8 {
        match self {
            Space::Holding => 3,
            Space::Input => 4,
        }
    }
}

impl std::fmt::Display for Space {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Space::Holding => "holding",
            Space::Input => "input",
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ExceptionCode(pub u8);

impl ExceptionCode {
    pub const ILLEGAL_DATA_ADDRESS: ExceptionCode = ExceptionCode(2);
}

impl std::fmt::Debug for ExceptionCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.0 {
            1 => "IllegalFunction",
            2 => "IllegalDataAddress",
            3 => "IllegalDataValue",
            4 => "ServerDeviceFailure",
            5 => "Acknowledge",
            6 => "ServerDeviceBusy",
            8 => "MemoryParityError",
            10 => "GatewayPathUnavailable",
            11 => "GatewayTargetDeviceFailedToRespond",
            code => return write!(f, "Unknown({code})"),
        })
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// Connecting, timeouts, I/O, and responses that make no sense (`ErrorKind::InvalidData`.)
    #[error("communication with the battery failed")]
    Transport(#[source] std::io::Error),
    #[error("the battery responded with an exception ({0:?})")]
    Exception(ExceptionCode),
}

pub(crate) fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0xFFFF_u16;
    for &byte in data {
        crc ^= u16::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { crc >> 1 ^ 0xA001 } else { crc >> 1 };
        }
    }
    crc
}

fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into())
}

type Reply = Result<Vec<u16>, ExceptionCode>;

/// Make sense of the PDU of a response (function code and what follows.)
fn parse_pdu(pdu: &[u8], function: u8, count: u16) -> std::io::Result<Reply> {
    match pdu {
        [f, code] if *f == function | 0x80 => Ok(Err(ExceptionCode(*code))),
        [f, bytes, data @ ..]
            if *f == function
                && usize::from(*bytes) == 2 * usize::from(count)
                && data.len() == usize::from(*bytes) =>
        {
            Ok(Ok(data.chunks_exact(2).map(|w| u16::from_be_bytes([w[0], w[1]])).collect()))
        }
        _ => Err(invalid(format!("unexpected response PDU {pdu:02x?}"))),
    }
}

struct Connection {
    stream: TcpStream,
}

impl Connection {
    /// Throw away what has arrived without having been asked for. Best effort.
    fn discard_stale(&self) -> std::io::Result<()> {
        let mut buffer = [0u8; 256];
        for _ in 0..64 {
            match self.stream.try_read(&mut buffer) {
                Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => debug!(bytes = n, "discarded data nobody has asked for"),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Discard what arrives until the line has been quiet for `quiet`. Returns `false` if it
    /// has not become quiet in a reasonable time.
    async fn drain(&mut self, quiet: Duration) -> std::io::Result<bool> {
        let started = Instant::now();
        let mut buffer = [0u8; 256];
        while started.elapsed() <= MAX_DRAIN {
            match tokio::time::timeout(quiet, self.stream.read(&mut buffer)).await {
                Err(_) => return Ok(true),
                Ok(Ok(0)) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                Ok(Ok(n)) => debug!(bytes = n, "discarded the rest of a response"),
                Ok(Err(e)) => return Err(e),
            }
        }
        Ok(false)
    }

    /// Send a request and wait for the response. `InvalidData` means that something has arrived
    /// that is not what was asked for. Not cancellation safe: drain or drop the connection after.
    async fn exchange(
        &mut self,
        unit_id: u8,
        function: u8,
        start: u16,
        count: u16,
    ) -> std::io::Result<Reply> {
        self.discard_stale()?;
        let mut frame = Vec::with_capacity(8);
        frame.extend([unit_id, function]);
        frame.extend(start.to_be_bytes());
        frame.extend(count.to_be_bytes());
        frame.extend(crc16(&frame).to_le_bytes());
        self.stream.write_all(&frame).await?;
        self.read_rtu(unit_id, function, count).await
    }

    async fn read_rtu(&mut self, unit_id: u8, function: u8, count: u16) -> std::io::Result<Reply> {
        // The first three bytes tell how long the rest is, so a made up length never makes us read
        // more than the protocol allows.
        let mut buffer = [0u8; 256];
        self.stream.read_exact(&mut buffer[..3]).await?;
        if buffer[0] != unit_id {
            return Err(invalid(format!("response from unit {}, not {unit_id}", buffer[0])));
        }
        let total = if buffer[1] == function | 0x80 {
            5
        } else if buffer[1] != function {
            return Err(invalid(format!("response with function code {}", buffer[1])));
        } else if usize::from(buffer[2]) != 2 * usize::from(count) {
            let bytes = buffer[2];
            return Err(invalid(format!("{bytes} bytes in response to a read of {count}")));
        } else {
            3 + usize::from(buffer[2]) + 2
        };
        self.stream.read_exact(&mut buffer[3..total]).await?;
        let (body, crc) = buffer[..total].split_at(total - 2);
        if crc16(body) != u16::from_le_bytes([crc[0], crc[1]]) {
            return Err(invalid(format!("response with a bad CRC {:02x?}", &buffer[..total])));
        }
        parse_pdu(&body[1..], function, count)
    }
}

/// Accepts `host`, `host:port`, `ip` and `ip:port` (IPv6 literals need brackets if there is a
/// port.) The port is 502 if not specified.
pub(crate) async fn resolve(tcp: &str) -> std::io::Result<SocketAddr> {
    if let Ok(address) = tcp.parse::<SocketAddr>() {
        return Ok(address);
    }
    if let Ok(ip) = tcp.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, 502));
    }
    let (host, port) = tcp
        .rsplit_once(':')
        .and_then(|(host, port)| Some((host, port.parse::<u16>().ok()?)))
        .unwrap_or((tcp, 502));
    tokio::net::lookup_host((host, port)).await?.next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "did not resolve to any addresses")
    })
}

pub struct Client {
    /// Resolved again whenever a connection is opened, in case the gateway has moved.
    tcp: String,
    unit_id: u8,
    timeout: Duration,
    request_gap: Duration,
    connection: Option<Connection>,
    last_exchange: Option<Instant>,
    pub(crate) quiet: Duration,
}

impl Client {
    /// Connects on the first request. `tcp` is as in [`resolve`].
    pub fn new(tcp: String, unit_id: u8, timeout: Duration, request_gap: Duration) -> Client {
        let (connection, last_exchange, quiet) = (None, None, QUIET);
        Client { tcp, unit_id, timeout, request_gap, connection, last_exchange, quiet }
    }

    async fn connect(&mut self) -> std::io::Result<()> {
        if self.connection.is_some() {
            return Ok(());
        }
        let opening = async {
            let stream = TcpStream::connect(resolve(&self.tcp).await?).await?;
            stream.set_nodelay(true)?;
            Ok::<_, std::io::Error>(stream)
        };
        let stream = tokio::time::timeout(self.timeout, opening)
            .await
            .unwrap_or_else(|_| Err(std::io::ErrorKind::TimedOut.into()))?;
        debug!("connected to the battery");
        self.connection = Some(Connection { stream });
        Ok(())
    }

    /// Read out registers. Anything but a proper response, an exception included, is an error.
    /// Takes `&mut self` as the bus can only deal with one request at a time.
    pub async fn read(&mut self, space: Space, start: u16, count: u16) -> Result<Vec<u16>, Error> {
        if !(1..=MAX_COUNT).contains(&count) || u32::from(start) + u32::from(count) > 0x1_0000 {
            let message = format!("cannot read {count} registers at {start}");
            let error = std::io::Error::new(std::io::ErrorKind::InvalidInput, message);
            return Err(Error::Transport(error));
        }
        if let Some(last) = self.last_exchange {
            tokio::time::sleep_until(last + self.request_gap).await;
        }
        self.connect().await.map_err(Error::Transport)?;
        let started = Instant::now();
        let connection = self.connection.as_mut().expect("connected above");
        let exchange = connection.exchange(self.unit_id, space.function(), start, count);
        let result = match tokio::time::timeout(self.timeout, exchange).await {
            Ok(Ok(reply)) => {
                debug!(%space, start, count, elapsed = ?started.elapsed(), "request completed");
                reply.map_err(Error::Exception)
            }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::InvalidData => {
                self.resynchronize().await;
                Err(Error::Transport(e))
            }
            Ok(Err(e)) => {
                self.connection = None;
                Err(Error::Transport(e))
            }
            Err(_) => {
                self.resynchronize().await;
                Err(Error::Transport(std::io::ErrorKind::TimedOut.into()))
            }
        };
        self.last_exchange = Some(Instant::now());
        result
    }

    /// After a request that did not go well, get to where the next response is going to be to
    /// the next request. There is no telling where one RTU frame ends and the other begins other
    /// than by waiting for silence.
    async fn resynchronize(&mut self) {
        let quiet = self.quiet;
        let drained = match &mut self.connection {
            Some(connection) => connection.drain(quiet).await,
            None => Ok(false),
        };
        if !matches!(drained, Ok(true)) {
            self.connection = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modbus::fake::{self, Step};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    fn client(address: SocketAddr) -> Client {
        let (timeout, gap) = (Duration::from_millis(250), Duration::from_millis(1));
        let mut client = Client::new(address.to_string(), 1, timeout, gap);
        client.quiet = Duration::from_millis(50);
        client
    }

    fn is_transport<T: std::fmt::Debug>(result: Result<T, Error>) -> bool {
        matches!(result, Err(Error::Transport(_)))
    }

    #[test]
    fn crc_of_a_known_frame() {
        let crc = crc16(&[0x01, 0x03, 0x00, 0x00, 0x00, 0x01]);
        assert_eq!(crc.to_le_bytes(), [0x84, 0x0A]);
    }

    #[tokio::test]
    async fn round_trips() {
        let (address, _) = fake::start(fake::ok).await;
        let mut client = client(address);
        assert_eq!(client.read(Space::Input, 10, 3).await.unwrap(), [4010, 4011, 4012]);
        assert_eq!(client.read(Space::Holding, 0xFFFF, 1).await.unwrap(), [2999]);
        assert_eq!(client.read(Space::Holding, 1, 125).await.unwrap().len(), 125);
    }

    #[tokio::test]
    async fn exceptions_are_reported_and_cost_nothing() {
        let accepted = Arc::new(AtomicUsize::new(0));
        let (address, _) = fake::start_counting(accepted.clone(), |request| match request.n {
            0 => vec![Step::Send(request.exception(2))],
            _ => fake::ok(request),
        })
        .await;
        let mut client = client(address);
        let failed = client.read(Space::Holding, 1, 1).await;
        assert!(matches!(failed, Err(Error::Exception(ExceptionCode::ILLEGAL_DATA_ADDRESS))));
        assert_eq!(client.read(Space::Holding, 1, 1).await.unwrap(), [3001]);
        assert_eq!(accepted.load(SeqCst), 1);
    }

    #[tokio::test]
    async fn rtu_bad_crc_and_trailing_garbage_do_not_desynchronize() {
        let (address, _) = fake::start(|request| match request.n {
            0 => {
                let mut frame = request.data_frame();
                *frame.last_mut().unwrap() ^= 0x55;
                let pause = Step::Wait(Duration::from_millis(15));
                vec![
                    Step::Send(frame),
                    pause,
                    Step::Send(vec![1, 3, 2, 0xDE]),
                    Step::Wait(Duration::from_millis(15)),
                    Step::Send(vec![0xAD, 0xBE]),
                ]
            }
            _ => fake::ok(request),
        })
        .await;
        let mut client = client(address);
        let failed = client.read(Space::Holding, 1, 1).await;
        assert!(
            matches!(&failed, Err(Error::Transport(e)) if e.kind() == std::io::ErrorKind::InvalidData)
        );
        assert_eq!(client.read(Space::Holding, 1, 1).await.unwrap(), [3001]);
        assert_eq!(client.read(Space::Holding, 2, 1).await.unwrap(), [3002]);
    }

    #[tokio::test]
    async fn rtu_nonsense_responses_are_rejected() {
        let (address, _) = fake::start(|request| {
            let frame = match request.n {
                0 => request.frame_from(2, &[3, 2, 0, 7]), // another unit
                1 => request.frame_from(1, &[3, 4, 0, 7, 0, 8]), // the wrong number of bytes
                2 => request.frame_from(1, &[4, 2, 0, 7]), // the wrong function code
                3 => vec![1, 3, 0xFF, 0, 0, 0, 0],         // a huge byte count
                4 => request.data_frame()[..4].to_vec(),   // cut short
                _ => return fake::ok(request),
            };
            vec![Step::Send(frame)]
        })
        .await;
        let mut client = client(address);
        for _ in 0..5 {
            assert!(is_transport(client.read(Space::Holding, 1, 1).await));
        }
        assert_eq!(client.read(Space::Holding, 1, 1).await.unwrap(), [3001]);
    }

    #[tokio::test]
    async fn rtu_timeout_and_the_late_response_do_not_hurt_the_next_request() {
        let (address, _) = fake::start(|request| match request.n {
            0 => vec![Step::Wait(Duration::from_millis(300)), Step::Send(request.data_frame())],
            _ => fake::ok(request),
        })
        .await;
        let mut client = client(address);
        client.quiet = Duration::from_millis(150);
        assert!(is_transport(client.read(Space::Holding, 5, 1).await));
        // The late response is to another register, so mistaking it for the response would show.
        assert_eq!(client.read(Space::Holding, 6, 1).await.unwrap(), [3006]);
    }

    #[tokio::test]
    async fn a_connection_that_cannot_be_trusted_is_opened_again() {
        let accepted = Arc::new(AtomicUsize::new(0));
        let (address, _) = fake::start_counting(accepted.clone(), |request| match request.n {
            0 => vec![Step::Close],
            _ => fake::ok(request),
        })
        .await;
        let mut client = client(address);
        assert!(is_transport(client.read(Space::Holding, 1, 1).await));
        assert_eq!(client.read(Space::Holding, 1, 1).await.unwrap(), [3001]);
        assert_eq!(accepted.load(SeqCst), 2);
    }

    #[tokio::test]
    async fn requests_are_spaced_out_and_bad_ones_not_sent() {
        let arrivals = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = arrivals.clone();
        let (address, _) = fake::start(move |request| {
            seen.lock().unwrap().push(Instant::now());
            fake::ok(request)
        })
        .await;
        let mut client = client(address);
        client.request_gap = Duration::from_millis(100);
        for _ in 0..3 {
            client.read(Space::Holding, 1, 1).await.unwrap();
        }
        let arrivals = arrivals.lock().unwrap().clone();
        assert!(arrivals.windows(2).all(|w| w[1] - w[0] >= Duration::from_millis(95)));

        for (start, count) in [(0, 0), (0, 126), (65535, 2)] {
            assert!(is_transport(client.read(Space::Holding, start, count).await));
        }
        assert_eq!(arrivals.len(), 3);
    }

    #[tokio::test]
    async fn nothing_listening_is_an_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = client(listener.local_addr().unwrap());
        drop(listener);
        assert!(is_transport(client.read(Space::Holding, 1, 1).await));
    }
}
