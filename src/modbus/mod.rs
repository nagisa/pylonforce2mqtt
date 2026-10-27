//! The Modbus side of things: polls blocks of registers, keeps the [`Snapshot`] of their contents
//! up to date and signals when it (or the availability of the battery) changes. It does not know
//! what the registers mean.

mod client;
#[cfg(test)]
pub(crate) mod fake;
mod snapshot;

pub use client::{Client, Error as RequestError, ExceptionCode, MAX_COUNT, Space};
pub use snapshot::Snapshot;

use std::sync::{Arc, RwLock, RwLockReadGuard};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;
use tracing::{debug, info, warn};

/// How often a block that the battery has nothing at is tried again. The battery may have fewer
/// modules than the block covers, but modules may also be added.
const RETRY_MISSING_EVERY: Duration = Duration::from_secs(10 * 60);

/// How soon a block that could not be read is tried again: right away the first time, then `base`,
/// twice that and so on up to `max` (and never more than the block's period.)
#[derive(Debug, Clone, Copy)]
struct Backoff {
    base: Duration,
    max: Duration,
}

impl Backoff {
    const DEFAULT: Backoff = Backoff { base: Duration::from_secs(1), max: Duration::from_secs(10) };

    /// The delay after this many failures in a row.
    fn delay(&self, failures: u32) -> Duration {
        match failures {
            0 | 1 => Duration::ZERO,
            n => self.base.saturating_mul(1 << (n - 2).min(16)).min(self.max),
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("lookup of `{1}` failed")]
    LookupHost(#[source] std::io::Error, String),
    #[error("polling {0} registers at {1} would go past the end of the address space")]
    BlockOutOfRange(u16, u16),
    #[error("more than one read of {1} {0} registers: the responses could not be told apart")]
    SameSizedReads(Space, u16),
}

#[derive(clap::Parser, Clone)]
#[group(id = "modbus::Args")]
pub struct Args {
    /// The address of the RS485 gateway that the battery is reached through.
    ///
    /// Given as `host` or `host:port` (IPv6 literals need brackets if there is a port.) The port
    /// is 502 if not specified.
    #[arg(long, short = 't')]
    tcp: String,

    /// The Modbus unit ID (slave address) of the battery.
    #[arg(long, short = 'i', default_value_t = 1)]
    unit_id: u8,

    /// How long to wait for the battery to respond to a request.
    ///
    /// A request that is not responded to is not given up on (and the same registers asked for
    /// again) any sooner: RTU responses can only be told apart by their length, so there is not to
    /// be more than one response of a length on its way at a time.
    #[arg(long, default_value = "5s")]
    timeout: humantime::Duration,

    /// How long to leave the bus alone between consecutive requests.
    #[arg(long, default_value = "50ms")]
    request_gap: humantime::Duration,

    /// How long the battery has to go without a single successful read to be considered
    /// unavailable.
    #[arg(long, default_value = "60s")]
    unavailable_after: humantime::Duration,
}

/// A block of registers to poll every `period`. Blocks longer than a request can read are split.
#[derive(Debug, Clone, Copy)]
pub struct PollBlock {
    pub space: Space,
    pub start: u16,
    pub count: u16,
    pub period: Duration,
}

struct Shared {
    snapshot: RwLock<Snapshot>,
    changed: Notify,
    unavailable_after: Duration,
}

impl Shared {
    fn read(&self) -> RwLockReadGuard<'_, Snapshot> {
        self.snapshot.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Snapshot> {
        self.snapshot.write().unwrap_or_else(|e| e.into_inner())
    }

    fn expires_at(&self) -> Option<Instant> {
        self.read().expires_at(self.unavailable_after)
    }

    /// Make the battery unavailable if it is overdue for that.
    fn expire(&self) {
        let mut snapshot = self.write();
        if snapshot.expires_at(self.unavailable_after).is_some_and(|at| at <= Instant::now()) {
            snapshot.set_available(false);
            drop(snapshot);
            warn!(after = ?self.unavailable_after, "no reads have succeeded for a while, the battery is unavailable");
            self.changed.notify_one();
        }
    }
}

#[derive(Clone)]
pub struct Handle {
    shared: Arc<Shared>,
    /// Lets the worker know when all of the handles are gone.
    _alive: mpsc::Sender<()>,
}

impl Handle {
    /// A handle that is not connected to anything, for tests.
    #[cfg(test)]
    pub(crate) fn detached(snapshot: Snapshot) -> Handle {
        let shared = Shared {
            snapshot: RwLock::new(snapshot),
            changed: Notify::new(),
            unavailable_after: Duration::ZERO,
        };
        Handle { shared: Arc::new(shared), _alive: mpsc::channel(1).0 }
    }

    /// The current view of the registers. Do not hold on to it: it blocks the updates.
    pub fn snapshot(&self) -> RwLockReadGuard<'_, Snapshot> {
        self.shared.read()
    }

    #[cfg(test)]
    pub(crate) fn snapshot_mut(&self) -> std::sync::RwLockWriteGuard<'_, Snapshot> {
        self.shared.write()
    }

    /// Wait for a poll to store a different or new word, or for the battery to become available
    /// or unavailable. There is one consumer; changes while nobody waits are remembered as one.
    pub async fn changed(&self) {
        self.shared.changed.notified().await;
    }
}

pub struct Device {
    pub handle: Handle,
    /// Completes once all of the handles are gone. Connection problems do not end it.
    pub task: tokio::task::JoinHandle<()>,
}

/// Resolve the address of the gateway, so that a mistake in it is found out early. It is resolved
/// again whenever a connection is opened, so the gateway need not stay where it is.
async fn client(args: &Args) -> Result<Client, Error> {
    let address =
        client::resolve(&args.tcp).await.map_err(|e| Error::LookupHost(e, args.tcp.clone()))?;
    info!(%address, "gateway address resolved");
    Ok(Client::new(args.tcp.clone(), args.unit_id, *args.timeout, *args.request_gap))
}

/// Start polling the blocks of registers. The battery need not be reachable: polls fail until it is.
pub async fn connect(args: &Args, blocks: Vec<PollBlock>) -> Result<Device, Error> {
    connect_tuned(args, blocks, Tuning::default()).await
}

/// The timing of things that the tests need to speed up.
struct Tuning {
    retry_missing_every: Duration,
    backoff: Backoff,
    quiet: Option<Duration>,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning { retry_missing_every: RETRY_MISSING_EVERY, backoff: Backoff::DEFAULT, quiet: None }
    }
}

async fn connect_tuned(
    args: &Args,
    blocks: Vec<PollBlock>,
    tuning: Tuning,
) -> Result<Device, Error> {
    let mut client = client(args).await?;
    if let Some(quiet) = tuning.quiet {
        client.quiet = quiet;
    }
    let blocks = split(blocks)?;
    check_sizes(&blocks)?;
    info!(blocks = blocks.len(), "polling plan");
    let shared = Shared {
        snapshot: RwLock::default(),
        changed: Notify::new(),
        unavailable_after: *args.unavailable_after,
    };
    let shared = Arc::new(shared);
    let (alive, handles) = mpsc::channel(1);
    let worker = Worker {
        client,
        shared: Arc::clone(&shared),
        blocks,
        retry_missing_every: tuning.retry_missing_every,
        backoff: tuning.backoff,
    };
    let task = tokio::task::spawn(worker.run(handles));
    Ok(Device { handle: Handle { shared, _alive: alive }, task })
}

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Ok,
    /// The battery has nothing at some of the addresses.
    Missing,
    /// The last poll failed; the number of those in a row that got no (sensible) response.
    Failing(u32),
}

/// A block, or a part of one that a single request can read.
struct Block {
    space: Space,
    start: u16,
    count: u16,
    period: Duration,
    next_due: Instant,
    status: Status,
}

impl std::fmt::Display for Block {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let end = u32::from(self.start) + u32::from(self.count) - 1;
        write!(f, "{} registers {}..={end}", self.space, self.start)
    }
}

/// Every read has to be of a different number of registers (or of the other space), as that is
/// the only way to tell which request a response is to. See [`client::Late`].
fn check_sizes(blocks: &[Block]) -> Result<(), Error> {
    let mut seen = std::collections::BTreeSet::new();
    match blocks.iter().find(|block| !seen.insert((block.space, block.count))) {
        Some(block) => Err(Error::SameSizedReads(block.space, block.count)),
        None => Ok(()),
    }
}

fn split(blocks: Vec<PollBlock>) -> Result<Vec<Block>, Error> {
    let now = Instant::now();
    let mut parts = Vec::new();
    for PollBlock { space, start, count, period } in blocks {
        let end = u32::from(start) + u32::from(count);
        if end > 0x1_0000 {
            return Err(Error::BlockOutOfRange(count, start));
        }
        for start in (u32::from(start)..end).step_by(MAX_COUNT.into()) {
            let count = (end - start).min(MAX_COUNT.into()) as u16;
            let (start, next_due, status) = (start as u16, now, Status::Ok);
            parts.push(Block { space, start, count, period, next_due, status });
        }
    }
    Ok(parts)
}

async fn sleep_until_or_never(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

struct Worker {
    client: Client,
    shared: Arc<Shared>,
    blocks: Vec<Block>,
    retry_missing_every: Duration,
    backoff: Backoff,
}

impl Worker {
    /// Poll the blocks one by one, whichever is due first. Runs until the handles are gone.
    async fn run(mut self, mut handles: mpsc::Receiver<()>) {
        loop {
            let next = self.blocks.iter().enumerate().min_by_key(|(_, block)| block.next_due);
            let next = next.map(|(index, block)| (index, block.next_due));
            tokio::select! {
                // Nothing is ever sent: this completes once the last handle is dropped.
                _ = handles.recv() => return,
                () = sleep_until_or_never(next.map(|(_, due)| due)) => {
                    self.poll(next.expect("woken up by a block being due").0).await;
                }
                () = sleep_until_or_never(self.shared.expires_at()) => self.shared.expire(),
            }
        }
    }

    async fn poll(&mut self, index: usize) {
        let Block { space, start, count, .. } = self.blocks[index];
        let started = Instant::now();
        let result = {
            let read = self.client.read(space, start, count);
            tokio::pin!(read);
            // A request cannot be given up on half way (opening the connection can take a while),
            // but the battery becoming unavailable is not to wait for it.
            loop {
                tokio::select! {
                    result = &mut read => break result,
                    () = sleep_until_or_never(self.shared.expires_at()) => self.shared.expire(),
                }
            }
        };
        self.finish(index, started, result);
        let now = Instant::now();
        for (space, words) in self.client.take_late() {
            let count = words.len();
            match self.blocks.iter().find(|b| b.space == space && usize::from(b.count) == count) {
                Some(block) => self.store(space, block.start, &words, now),
                None => debug!(%space, count, "a late response that is to none of the blocks"),
            }
        }
    }

    /// Put what has been read into the snapshot, and let it be known if that changes anything.
    fn store(&self, space: Space, start: u16, words: &[u16], now: Instant) {
        let mut snapshot = self.shared.write();
        let mut stored = false;
        for (i, &word) in words.iter().enumerate() {
            stored |= snapshot.set(space, start + i as u16, word);
        }
        let available = snapshot.record_success(now);
        drop(snapshot);
        if available {
            info!("the battery is available");
        }
        if stored || available {
            debug!(%space, start, available, "registers changed");
            self.shared.changed.notify_one();
        }
    }

    fn finish(&mut self, index: usize, started: Instant, result: Result<Vec<u16>, RequestError>) {
        let now = Instant::now();
        let block = &mut self.blocks[index];
        block.next_due = (block.next_due + block.period).max(now);
        let was = block.status;
        match result {
            Ok(words) => {
                match was {
                    Status::Ok => {}
                    Status::Missing => {
                        info!(%block, "the battery has something at these addresses now")
                    }
                    Status::Failing(_) => info!(%block, "the registers can be read out again"),
                }
                block.status = Status::Ok;
                debug!(%block, took = ?started.elapsed(), "read out");
                let (space, start) = (block.space, block.start);
                self.store(space, start, &words, now);
            }
            // The battery responded properly. It just does not have what is asked for.
            Err(RequestError::Exception(ExceptionCode::ILLEGAL_DATA_ADDRESS)) => {
                if was != Status::Missing {
                    let retry_in = self.retry_missing_every;
                    info!(%block, ?retry_in, "the battery has nothing at some of these addresses");
                }
                block.status = Status::Missing;
                block.next_due = now + self.retry_missing_every;
            }
            Err(e) => {
                if !matches!(was, Status::Failing(_)) {
                    warn!(error = (&e as &dyn std::error::Error), %block, "could not read out the registers");
                }
                block.status = Status::Failing(match (&e, was) {
                    // The battery has said its piece, and will say the same if asked at once.
                    (RequestError::Exception(_), _) => 0,
                    (_, Status::Failing(n)) => n.saturating_add(1),
                    _ => 1,
                });
                if let (Status::Failing(n), RequestError::Transport(_)) = (block.status, &e) {
                    // Not a word from the battery: not to wait for the whole period, and not to
                    // be asked again too eagerly either.
                    block.next_due = now + self.backoff.delay(n).min(block.period);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;
    use fake::{Req, Step};
    use std::sync::atomic::{AtomicU16, Ordering::SeqCst};
    use std::sync::{Arc, Mutex};

    #[derive(clap::Parser)]
    struct Cli {
        #[clap(flatten)]
        modbus: Args,
    }

    fn args(address: std::net::SocketAddr, unavailable_after: &str) -> Args {
        let address = address.to_string();
        let argv = ["test", "--tcp", &address, "--timeout", "100ms", "--request-gap", "1ms"];
        let argv = argv.into_iter().chain(["--unavailable-after", unavailable_after]);
        Cli::try_parse_from(argv).unwrap().modbus
    }

    fn tuning() -> Tuning {
        let backoff = Backoff { base: Duration::from_millis(5), max: Duration::from_millis(20) };
        let (retry_missing_every, quiet) =
            (Duration::from_secs(600), Some(Duration::from_millis(20)));
        Tuning { retry_missing_every, backoff, quiet }
    }

    fn block(space: Space, start: u16, count: u16) -> PollBlock {
        PollBlock { space, start, count, period: Duration::from_millis(20) }
    }

    async fn changed_within(handle: &Handle, millis: u64) -> bool {
        tokio::time::timeout(Duration::from_millis(millis), handle.changed()).await.is_ok()
    }

    type Arrivals = Arc<Mutex<Vec<(Instant, u16)>>>;

    /// Wrap a responder to record when each request (and for what address) has arrived.
    fn recording(
        respond: impl Fn(&Req) -> Vec<Step> + Send + Sync + 'static,
    ) -> (Arrivals, impl Fn(&Req) -> Vec<Step> + Send + Sync + 'static) {
        let arrivals = Arrivals::default();
        let seen = Arc::clone(&arrivals);
        let respond = move |request: &Req| {
            seen.lock().unwrap().push((Instant::now(), request.start));
            respond(request)
        };
        (arrivals, respond)
    }

    #[test]
    fn long_blocks_are_split_and_delays_back_off() {
        let blocks = split(vec![block(Space::Input, 100, 300), block(Space::Holding, 0, 5)]);
        let ranges: Vec<_> = blocks.unwrap().iter().map(|b| (b.space, b.start, b.count)).collect();
        let expected =
            [(Space::Input, 100, 125), (Space::Input, 225, 125), (Space::Input, 350, 50)];
        assert_eq!(ranges, [expected[0], expected[1], expected[2], (Space::Holding, 0, 5)]);
        let blocks = split(vec![block(Space::Input, 65400, 136)]).unwrap();
        assert_eq!(blocks.iter().map(|b| b.count).collect::<Vec<_>>(), [125, 11]);
        assert!(split(vec![block(Space::Input, 65400, 137)]).is_err());

        let delays: Vec<_> = (0..8).map(|n| Backoff::DEFAULT.delay(n).as_secs()).collect();
        assert_eq!(delays, [0, 0, 1, 2, 4, 8, 10, 10]);
        assert_eq!(Backoff::DEFAULT.delay(u32::MAX), Duration::from_secs(10));
    }

    #[test]
    fn reads_of_the_same_size_are_refused() {
        let same = split(vec![block(Space::Holding, 1, 6), block(Space::Holding, 100, 6)]);
        assert!(matches!(check_sizes(&same.unwrap()), Err(Error::SameSizedReads(_, 6))));
        let other_space = split(vec![block(Space::Holding, 1, 6), block(Space::Input, 100, 6)]);
        assert!(check_sizes(&other_space.unwrap()).is_ok());
        let parts = split(vec![block(Space::Input, 100, 300)]).unwrap();
        assert!(matches!(check_sizes(&parts), Err(Error::SameSizedReads(_, 125))));
    }

    #[tokio::test]
    async fn a_late_response_goes_to_the_block_of_its_length() {
        // The block at 100 is never responded to in time, but its response turns up with that of
        // the other one.
        let (address, _server) = fake::start(|request| match request.start {
            100 => vec![],
            _ => {
                let late = Req { start: 100, count: 3, ..request.clone() }.data_frame();
                [Step::Send(late)].into_iter().chain(fake::ok(request)).collect()
            }
        })
        .await;
        let blocks = vec![block(Space::Holding, 1, 2), block(Space::Holding, 100, 3)];
        let device = connect_tuned(&args(address, "60s"), blocks, tuning()).await.unwrap();
        while device.handle.snapshot().words(Space::Holding, 100, 3).is_none() {
            assert!(changed_within(&device.handle, 2000).await);
        }
        let snapshot = device.handle.snapshot();
        assert_eq!(snapshot.words(Space::Holding, 100, 3), Some(vec![3100, 3101, 3102]));
        assert_eq!(snapshot.words(Space::Holding, 1, 2), Some(vec![3001, 3002]));
        drop(snapshot);
        device.task.abort();
    }

    #[tokio::test]
    async fn polling_fills_the_snapshot_and_only_changes_notify() {
        let offset = Arc::new(AtomicU16::new(0));
        let shift = Arc::clone(&offset);
        let (address, _server) = fake::start(move |request| {
            let shift = shift.load(SeqCst);
            vec![Step::Send(request.data_frame_with(request.words().iter().map(|w| w + shift)))]
        })
        .await;
        let blocks = vec![block(Space::Input, 100, 3), block(Space::Holding, 7, 2)];
        let device = connect_tuned(&args(address, "60s"), blocks, tuning()).await.unwrap();
        let handle = &device.handle;
        assert!(!handle.snapshot().available());

        assert!(changed_within(handle, 2000).await);
        while handle.snapshot().words(Space::Holding, 7, 2).is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Drop the permit stored for the second block's words that may not have been waited for.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let _ = changed_within(handle, 1).await;
        assert!(handle.snapshot().available());
        assert_eq!(handle.snapshot().words(Space::Input, 100, 3), Some(vec![4100, 4101, 4102]));
        assert_eq!(handle.snapshot().get(Space::Holding, 8), Some(3008));
        assert!(!changed_within(handle, 200).await, "nothing has changed");

        offset.store(1, SeqCst);
        while handle.snapshot().get(Space::Holding, 8) != Some(3009) {
            assert!(changed_within(handle, 2000).await);
        }
        assert_eq!(handle.snapshot().get(Space::Input, 100), Some(4101));

        let Device { handle, task } = device;
        let other = handle.clone();
        drop(handle);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!task.is_finished());
        drop(other);
        tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn availability_follows_the_battery_coming_and_going() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let serve = |listener| fake::serve(listener, Arc::default(), Arc::new(fake::ok));
        let server = serve(listener);
        let blocks = vec![block(Space::Input, 1, 1)];
        let device = connect_tuned(&args(address, "300ms"), blocks, tuning()).await.unwrap();
        let handle = &device.handle;

        assert!(changed_within(handle, 2000).await);
        assert!(handle.snapshot().available());

        server.abort();
        let _ = server.await;
        let gone = Instant::now();
        assert!(changed_within(handle, 2000).await, "no notification of the battery going away");
        assert!(!handle.snapshot().available());
        assert!(gone.elapsed() > Duration::from_millis(100), "{:?}", gone.elapsed());
        assert_eq!(handle.snapshot().get(Space::Input, 1), Some(4001));

        let _server = serve(tokio::net::TcpListener::bind(address).await.unwrap());
        assert!(changed_within(handle, 3000).await, "no notification of the battery coming back");
        assert!(handle.snapshot().available());
        device.task.abort();
    }

    #[tokio::test]
    async fn blocks_with_nothing_at_their_addresses_are_left_alone_for_a_while() {
        let (arrivals, respond) = recording(|request| match request.start {
            500 => vec![Step::Send(request.exception(2))],
            _ => fake::ok(request),
        });
        let (address, _server) = fake::start(respond).await;
        let blocks = vec![block(Space::Holding, 500, 10), block(Space::Holding, 1, 1)];
        let tuning = Tuning { retry_missing_every: Duration::from_millis(400), ..tuning() };
        let device = connect_tuned(&args(address, "60s"), blocks, tuning).await.unwrap();
        let count = |start| arrivals.lock().unwrap().iter().filter(|a| a.1 == start).count();

        tokio::time::sleep(Duration::from_millis(250)).await;
        // Every 20 milliseconds would be a dozen of them by now.
        assert_eq!(count(500), 1);
        assert!(count(1) >= 5, "{}", count(1));
        // The battery is available because of the other block.
        assert!(device.handle.snapshot().available());
        assert_eq!(device.handle.snapshot().get(Space::Holding, 500), None);

        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!((2..=3).contains(&count(500)), "{}", count(500));
        device.task.abort();
    }

    #[tokio::test]
    async fn a_block_that_cannot_be_read_is_tried_again_with_increasing_delays() {
        // The connection is lost with the first four requests, and with the seventh and eighth.
        let (arrivals, respond) = recording(|request| match request.n {
            0..4 | 6 | 7 => vec![Step::Close],
            _ => fake::ok(request),
        });
        let (address, _server) = fake::start(respond).await;
        let period = Duration::from_millis(300);
        let blocks = vec![PollBlock { period, ..block(Space::Holding, 1, 2) }];
        let backoff = Backoff { base: Duration::from_millis(100), max: Duration::from_millis(250) };
        let tuning = Tuning { backoff, ..tuning() };
        let device = connect_tuned(&args(address, "60s"), blocks, tuning).await.unwrap();
        while arrivals.lock().unwrap().len() < 9 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        device.task.abort();
        let arrivals = arrivals.lock().unwrap();
        let gaps: Vec<_> = arrivals.windows(2).map(|w| w[1].0 - w[0].0).collect();
        let (ms, right_away) = (Duration::from_millis, Duration::from_millis(80));
        // Right away the first time, and then the backoff, with no delay on top of the schedule
        // for opening the connection anew.
        assert!(gaps[0] < right_away, "{gaps:?}");
        assert!(gaps[1] >= ms(90) && gaps[1] < ms(190), "{gaps:?}");
        assert!(gaps[2] >= ms(190) && gaps[2] < ms(390), "{gaps:?}");
        assert!(gaps[3] >= ms(240) && gaps[3] < ms(290), "{gaps:?}");
        assert!(gaps[4] >= period - ms(20) && gaps[4] < period + right_away, "{gaps:?}");
        assert!(gaps[5] >= period - ms(20), "{gaps:?}");
        assert!(gaps[6] < right_away, "{gaps:?}");
        assert!(gaps[7] >= ms(90) && gaps[7] < ms(190), "{gaps:?}");
    }

    #[tokio::test]
    async fn the_backoff_is_capped_by_the_period() {
        let (arrivals, respond) = recording(|_| vec![Step::Close]);
        let (address, _server) = fake::start(respond).await;
        let blocks =
            vec![PollBlock { period: Duration::from_millis(30), ..block(Space::Holding, 1, 2) }];
        let backoff = Backoff { base: Duration::from_millis(500), max: Duration::from_secs(5) };
        let tuning = Tuning { backoff, ..tuning() };
        let device = connect_tuned(&args(address, "60s"), blocks, tuning).await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        device.task.abort();
        // Without the period getting in the way there would be three.
        assert!(arrivals.lock().unwrap().len() >= 6, "{}", arrivals.lock().unwrap().len());
    }

    #[tokio::test]
    async fn exceptions_are_not_retried_right_away_and_do_not_make_the_battery_available() {
        let (arrivals, respond) = recording(|request| vec![Step::Send(request.exception(4))]);
        let (address, _server) = fake::start(respond).await;
        let blocks =
            vec![PollBlock { period: Duration::from_millis(150), ..block(Space::Holding, 1, 2) }];
        let device = connect_tuned(&args(address, "60s"), blocks, tuning()).await.unwrap();
        assert!(!changed_within(&device.handle, 400).await);
        assert!(!device.handle.snapshot().available());
        device.task.abort();
        // At its period, and not any more eagerly because of it failing.
        let arrivals = arrivals.lock().unwrap().len();
        assert!((2..=3).contains(&arrivals), "{arrivals}");
    }
}
