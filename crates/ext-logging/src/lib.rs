//! Essential daemon facts, independent of the session ledger. Only the sealed
//! [`Loggable`] vocabulary can cross this boundary; formatting and disk I/O run
//! on `rt`'s blocking pool, and producers never wait for queue space or a lock.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use protocol::SessionId;
use serde::{Deserialize, Serialize, Serializer};

const CAPACITY: usize = 256;
/// Each of the live and two retained files is at most 256 KiB.
pub const MAX: u64 = 256 << 10;
pub const KEEP: usize = 2;
static CURRENT: OnceLock<Logger> = OnceLock::new();

mod sealed {
    pub trait Sealed {}
}

/// Closed to downstream implementations: `String`, payloads, error messages,
/// paths and arbitrary `Serialize` values cannot be passed to [`emit`].
///
/// ```compile_fail
/// ext_logging::emit(String::from("a prompt"));
/// ```
pub trait Loggable: sealed::Sealed + Into<Fact> {}
impl sealed::Sealed for Fact {}
impl Loggable for Fact {}

pub use sys::crash::Build;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Client,
    Foreground,
    /// The OS at login, through `a daemon autostart`.
    Login,
    /// The daemon of an earlier build, handing over to this one.
    Handover,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stop {
    Requested,
    Idle,
    Upgrade,
    Terminate,
    Interrupt,
    Hangup,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Rule {
    /// Reopen the durable ledger, then let the kernel resolve its intents.
    ResumeLedger,
    /// An actor abandoned its uncommitted effects and reopened its ledger.
    ReloadAfterFault,
    /// Only an unsealed final line may be truncated.
    CutTornTail,
    /// A sealed unreadable line or interior damage is preserved read-only.
    PreserveReadOnly,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Create,
    Commit,
    Reload,
    Snapshot,
    Catalog,
    Connection,
    Startup,
    Shutdown,
    /// Handing process groups to the next build, or adopting them from the
    /// one before.
    Handover,
}

/// `io::ErrorKind` carries no error message, path or source error.
#[derive(Clone, Copy, Debug)]
pub struct Fault(pub io::ErrorKind);
impl Serialize for Fault {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&format_args!("{:?}", self.0))
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Transport,
    Auth,
    RateLimit,
    Quota,
    ContextOverflow,
    InvalidRequest,
    ContentFilter,
    Server,
    Decode,
    Incomplete,
    Unsupported,
    Other,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogReason {
    Missing,
    Unclean,
    Unreadable,
}

/// No field admits free-form text. Add a named fact rather than an escape
/// hatch for messages: every field must remain bounded and content-free.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Fact {
    Started {
        build: Build,
        reset_breaker: bool,
        origin: Origin,
    },
    Stopped {
        reason: Stop,
    },
    PreviousCrash {
        pid: u32,
        build: Build,
    },
    BreakerTripped {
        young: u32,
    },
    Upgrade {
        to: Option<Build>,
    },
    /// The daemon handed its sessions' process groups to the next build.
    HandedOver {
        groups: usize,
    },
    Recovery {
        session: SessionId,
        rule: Rule,
    },
    StoreRepair {
        session: SessionId,
        rule: Rule,
        offset: u64,
    },
    CatalogRebuilt {
        reason: CatalogReason,
    },
    RegistryRejected,
    ConfigFallback {
        kind: Fault,
    },
    AdoptionRejected {
        session: SessionId,
        generation: u64,
    },
    RegistryWarnings {
        count: usize,
    },
    /// A compaction threshold is configured but the session's model has no
    /// known context window, so only provider overflows compact.
    ThresholdUnmeasured {
        session: SessionId,
    },
    ExtensionRejected {
        module: u64,
    },
    ClientObservation(Witness),
    Fault {
        session: Option<SessionId>,
        operation: Operation,
        kind: Fault,
    },
    KernelPanic {
        session: SessionId,
    },
    ShutdownExpired {
        sessions: usize,
    },
    Mismatch {
        min: u32,
        max: u32,
    },
    ProviderError {
        session: SessionId,
        kind: Provider,
        status: Option<u16>,
        retryable: bool,
    },
    Generation {
        workspace: u64,
        number: u64,
        accepted: bool,
    },
    Lost {
        queue: u64,
        writes: u64,
    },
}

/// Path correlation without recording a path (which can contain user text).
pub fn workspace(path: &std::path::Path) -> u64 {
    path.as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
        })
}

/// The last client observation survives a daemon unable to accept work. It
/// is folded into the next daemon's log, never appended by a second writer.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Observation {
    Unresponsive,
    Terminate,
    Kill,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Witness {
    pid: u32,
    started: u64,
    unix_ms: u64,
    reason: Observation,
}

pub fn witness(home: &sys::Home, daemon: &sys::crash::Record, reason: Observation) {
    let witness = Witness {
        pid: daemon.pid,
        started: daemon.started,
        unix_ms: now(),
        reason,
    };
    if let Ok(json) = json::Json::encode(&witness) {
        let _ = fs::write(home.run_dir().join("daemon.witness"), json.raw());
    }
}

pub fn replay_witness(home: &sys::Home) {
    if let Some(witness) = take_witness(home) {
        emit(Fact::ClientObservation(witness));
    }
}

fn take_witness(home: &sys::Home) -> Option<Witness> {
    let path = home.run_dir().join("daemon.witness");
    let read = || -> io::Result<Witness> {
        let mut text = String::new();
        fs::File::open(&path)?.take(512).read_to_string(&mut text)?;
        serde_json::from_str(&text).map_err(io::Error::other)
    };
    let witness = read().ok();
    let _ = fs::remove_file(path);
    witness
}

fn now() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

#[derive(Clone, Copy, Serialize)]
struct Record {
    unix_ms: u64,
    pid: u32,
    fact: Fact,
}
impl Record {
    fn new(fact: Fact) -> Self {
        Self {
            unix_ms: now(),
            pid: std::process::id(),
            fact,
        }
    }
}

struct Queue {
    records: Mutex<VecDeque<Record>>,
    ready: rt::Wakeup,
    drained: rt::Notify,
    submitted: AtomicU64,
    completed: AtomicU64,
    lost: AtomicU64,
    closed: AtomicBool,
    started: AtomicBool,
}

#[derive(Clone)]
struct Logger(Arc<Queue>);
impl Logger {
    fn new() -> Self {
        Self(Arc::new(Queue {
            records: Mutex::new(VecDeque::with_capacity(CAPACITY)),
            ready: rt::Wakeup::new(),
            drained: rt::Notify::new(),
            submitted: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            started: AtomicBool::new(false),
        }))
    }

    fn emit(&self, fact: impl Loggable) {
        if self.0.closed.load(Ordering::Acquire) {
            return;
        }
        // Even a consumer descheduled while holding this lock cannot delay
        // the session's commit or grow its memory use.
        if let Ok(mut records) = self.0.records.try_lock()
            && records.len() < CAPACITY
        {
            records.push_back(Record::new(fact.into()));
            self.0.submitted.fetch_add(1, Ordering::Release);
            drop(records);
            self.0.ready.notify();
            return;
        }
        self.0.lost.fetch_add(1, Ordering::Relaxed);
    }

    async fn flush(&self) {
        let target = self.0.submitted.load(Ordering::Acquire);
        let mut changed = self.0.drained.count();
        while self.0.completed.load(Ordering::Acquire) < target {
            self.0.drained.changed(&mut changed).await;
        }
    }

    fn run(&self, rt: &rt::Rt, mut write: impl FnMut(&[u8]) -> io::Result<()> + Send + 'static) {
        let queue = Arc::clone(&self.0);
        let executor = Arc::clone(rt);
        rt.spawn(Box::pin(async move {
            let mut failures = 0;
            loop {
                let batch: Vec<_> = queue.records.lock().unwrap().drain(..).collect();
                if batch.is_empty() {
                    if queue.closed.load(Ordering::Acquire) {
                        break;
                    }
                    queue.ready.wait().await;
                    continue;
                }
                let count = batch.len() as u64;
                let lost = queue.lost.swap(0, Ordering::Relaxed);
                let (returned, failed, pending) = rt::unblock_with(
                    &*executor,
                    rt::BlockingClass::Storage,
                    batch.len() * size_of::<Record>() + 4096,
                    move || {
                        let (mut pending, mut failed) = (lost, failures);
                        for record in batch {
                            if write_record(&mut write, record).is_err() {
                                failed += 1;
                            }
                        }
                        // Flush includes the loss count even when recovery was
                        // in the last batch and no further facts arrive.
                        if (pending != 0 || failed != 0)
                            && write_record(
                                &mut write,
                                Record::new(Fact::Lost {
                                    queue: pending,
                                    writes: failed,
                                }),
                            )
                            .is_ok()
                        {
                            pending = 0;
                            failed = 0;
                        }
                        (write, failed, pending)
                    },
                )
                .await;
                write = returned;
                failures = failed;
                queue.lost.fetch_add(pending, Ordering::Relaxed);
                queue.completed.fetch_add(count, Ordering::Release);
                queue.drained.notify();
            }
        }));
    }
}

fn write_record(write: &mut impl FnMut(&[u8]) -> io::Result<()>, record: Record) -> io::Result<()> {
    let json = json::Json::encode(&record).expect("facts encode");
    let mut line = Vec::with_capacity(json.raw().len() + 1);
    line.extend_from_slice(json.raw().as_bytes());
    line.push(b'\n');
    write(&line)
}

/// Installed once by a daemon after it owns its home lock. Ordinary clients
/// never install a writer, so they cannot race rotation with that daemon.
pub fn install(rt: &rt::Rt, home: &sys::Home) {
    let logger = CURRENT.get_or_init(Logger::new);
    if !logger.0.started.swap(true, Ordering::AcqRel) {
        let mut file = Rotating {
            path: home.daemon_log(),
            max: MAX,
            keep: KEEP,
            repair: None,
        };
        let native = sys::log::Native::for_home(home);
        logger.run(rt, move |line| {
            native.write(line);
            file.write(line)
        });
    }
}

pub fn emit(fact: impl Loggable) {
    CURRENT.get_or_init(Logger::new).emit(fact);
}

/// Logging and worker reclamation share this shutdown allowance.
pub const FLUSH_GRACE: Duration = Duration::from_millis(250);

/// Best effort only, including on orderly exit: a wedged disk never holds
/// daemon shutdown indefinitely. There is deliberately no flush in `Drop`.
pub async fn flush(rt: &dyn rt::Runtime) {
    flush_until(rt, rt.now() + FLUSH_GRACE).await;
}

/// The runtime owner uses this same deadline when reclaiming its workers.
pub async fn flush_until(rt: &dyn rt::Runtime, deadline: Instant) {
    if let Some(logger) = CURRENT.get() {
        logger.0.closed.store(true, Ordering::Release);
        logger.0.ready.notify();
        rt::or(
            logger.flush(),
            rt.sleep(deadline.saturating_duration_since(rt.now())),
        )
        .await;
    }
}

struct Rotating {
    path: PathBuf,
    max: u64,
    keep: usize,
    repair: Option<u64>,
}
impl Rotating {
    fn write(&mut self, line: &[u8]) -> io::Result<()> {
        if line.len() as u64 > self.max {
            return Err(io::ErrorKind::InvalidData.into());
        }
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        if let Some(offset) = self.repair {
            OpenOptions::new()
                .write(true)
                .open(&self.path)?
                .set_len(offset)?;
            self.repair = None;
        }
        repair_tail(&self.path)?;
        // Rotate before writing: neither an idle daemon nor a large burst
        // can grow a file beyond the bound. Reopening avoids stale inodes.
        sys::log::rotate(&self.path, self.max - line.len() as u64, self.keep)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.repair = Some(file.metadata()?.len());
        file.write_all(line)?;
        self.repair = None;
        Ok(())
    }
}

// A restarted writer has no in-memory offset for an interrupted append.
// Find the last delimiter without reading an unbounded file into memory.
fn repair_tail(path: &std::path::Path) -> io::Result<()> {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let original = file.metadata()?.len();
    let mut end = original;
    let mut bytes = [0; 1024];
    while end != 0 {
        let len = usize::try_from(end).unwrap_or(bytes.len()).min(bytes.len());
        let start = end - len as u64;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut bytes[..len])?;
        if let Some(at) = bytes[..len].iter().rposition(|b| *b == b'\n') {
            let sealed = start + at as u64 + 1;
            if sealed != original {
                file.set_len(sealed)?;
            }
            return Ok(());
        }
        end = start;
    }
    file.set_len(0)
}

#[cfg(test)]
mod tests;

/// A controlled disk stall for engine invariant tests; absent from release
/// builds. Its guard always releases the writer, including during unwinding.
#[cfg(feature = "testing")]
pub mod testing {
    use super::{CURRENT, Fact, Logger};
    /// Startup failures have no actor or installed writer. Inspect admitted
    /// facts directly in process-isolated tests, without altering admission.
    pub fn pending() -> Vec<Fact> {
        CURRENT
            .get_or_init(Logger::new)
            .0
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|record| record.fact)
            .collect()
    }

    pub struct Stall(Option<std::sync::mpsc::Sender<()>>);
    impl Drop for Stall {
        fn drop(&mut self) {
            drop(self.0.take());
        }
    }
    pub fn stall(rt: &rt::Rt) -> Stall {
        let logger = CURRENT.get_or_init(Logger::new);
        assert!(
            !logger
                .0
                .started
                .swap(true, std::sync::atomic::Ordering::AcqRel),
            "one daemon per process"
        );
        let (release, released) = std::sync::mpsc::channel();
        let (entered, waiting) = std::sync::mpsc::channel();
        let mut entered = Some(entered);
        logger.emit(Fact::RegistryRejected);
        logger.run(rt, move |_| {
            if let Some(entered) = entered.take() {
                let _ = entered.send(());
                let _ = released.recv();
            }
            Ok(())
        });
        waiting.recv().unwrap();
        Stall(Some(release))
    }
}
