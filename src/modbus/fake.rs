//! A fake battery on a local socket, for the tests.

use super::client::crc16;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

#[derive(Clone, Debug)]
pub(crate) struct Req {
    /// The number of this request among those made to the server (all connections included.)
    pub n: usize,
    pub unit: u8,
    pub function: u8,
    pub start: u16,
    pub count: u16,
}

pub(crate) enum Step {
    Send(Vec<u8>),
    Wait(Duration),
    Close,
}

impl Req {
    /// The words that the registers normally hold: `address + 1000 * function code`.
    pub fn words(&self) -> Vec<u16> {
        let base = 1000 * u16::from(self.function);
        (0..self.count).map(|i| (self.start + i).wrapping_add(base)).collect()
    }

    /// A frame with this PDU, coming from `unit`.
    pub fn frame_from(&self, unit: u8, pdu: &[u8]) -> Vec<u8> {
        let mut frame = vec![unit];
        frame.extend(pdu);
        frame.extend(crc16(&frame).to_le_bytes());
        frame
    }

    pub fn exception(&self, code: u8) -> Vec<u8> {
        self.frame_from(self.unit, &[self.function | 0x80, code])
    }

    /// A proper response to the request, with these words.
    pub fn data_frame_with(&self, words: impl IntoIterator<Item = u16>) -> Vec<u8> {
        let mut pdu = vec![self.function, (self.count * 2) as u8];
        pdu.extend(words.into_iter().flat_map(u16::to_be_bytes));
        self.frame_from(self.unit, &pdu)
    }

    pub fn data_frame(&self) -> Vec<u8> {
        self.data_frame_with(self.words())
    }
}

pub(crate) fn ok(request: &Req) -> Vec<Step> {
    vec![Step::Send(request.data_frame())]
}

pub(crate) type Respond = Arc<dyn Fn(&Req) -> Vec<Step> + Send + Sync>;

/// Start serving on a new address, counting the connections that are accepted.
pub(crate) async fn start_counting(
    accepted: Arc<AtomicUsize>,
    respond: impl Fn(&Req) -> Vec<Step> + Send + Sync + 'static,
) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    (listener.local_addr().unwrap(), serve(listener, accepted, Arc::new(respond)))
}

pub(crate) async fn start(
    respond: impl Fn(&Req) -> Vec<Step> + Send + Sync + 'static,
) -> (SocketAddr, JoinHandle<()>) {
    start_counting(Arc::default(), respond).await
}

/// Serve until the task is aborted, which also hangs up on all of the clients.
pub(crate) fn serve(
    listener: TcpListener,
    accepted: Arc<AtomicUsize>,
    respond: Respond,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (mut connections, requests) = (JoinSet::new(), Arc::new(AtomicUsize::new(0)));
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            accepted.fetch_add(1, SeqCst);
            let (requests, respond) = (Arc::clone(&requests), Arc::clone(&respond));
            connections.spawn(serve_connection(socket, requests, respond));
        }
    })
}

async fn serve_connection(mut socket: TcpStream, requests: Arc<AtomicUsize>, respond: Respond) {
    let word = |b: &[u8]| u16::from_be_bytes([b[0], b[1]]);
    loop {
        let mut frame = [0; 8];
        if socket.read_exact(&mut frame).await.is_err() {
            return;
        }
        let crc = crc16(&frame[..6]).to_le_bytes();
        assert_eq!(crc, frame[6..8], "bad CRC in {frame:02x?}");
        let (unit, function, start, count) =
            (frame[0], frame[1], word(&frame[2..]), word(&frame[4..]));
        let n = requests.fetch_add(1, SeqCst);
        let request = Req { n, unit, function, start, count };
        for step in respond(&request) {
            match step {
                Step::Send(bytes) => {
                    if socket.write_all(&bytes).await.is_err() {
                        return;
                    }
                }
                Step::Wait(duration) => tokio::time::sleep(duration).await,
                Step::Close => return,
            }
        }
    }
}
