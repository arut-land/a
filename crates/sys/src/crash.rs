//! What a dying daemon leaves behind: a report per crash under `crash/`,
//! and a [`Breaker`] that stops a daemon that keeps dying soon after it
//! starts from serving again.
//!
//! The running daemon locks `run/daemon.pid` and records itself in it
//! ([`Record`]); a clean exit empties it. The lock is taken by the client
//! that starts the daemon and handed to it, so it is never free while one
//! runs, and only the daemon holding it reaps or counts crashes. A record found unlocked is a
//! daemon that died without exiting: killed, aborted, or crashed by a
//! signal. A panic hook also writes the reason, when there is one.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::panic::{self, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::Home;
use serde::{Serialize, Serializer};

/// A staged build's numeric identity, never caller-supplied text.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Build {
    version: Option<[u32; 3]>,
    fingerprint: Option<u64>,
}
impl Build {
    pub fn of(id: &str) -> Self {
        let (version, hash) = id.split_once('+').unwrap_or((id, ""));
        let mut parts = version.split('.');
        let version = (|| {
            let value = [
                parts.next()?.parse().ok()?,
                parts.next()?.parse().ok()?,
                parts.next()?.parse().ok()?,
            ];
            parts.next().is_none().then_some(value)
        })();
        let fingerprint = (hash.len() == 16)
            .then(|| u64::from_str_radix(hash, 16).ok())
            .flatten();
        Self {
            version,
            fingerprint,
        }
    }
}

/// Crash reports kept, newest first.
const KEEP: usize = 20;

/// A daemon that dies within this long of starting counts toward the
/// breaker; one that lived longer starts the count again.
pub const YOUNG: Duration = Duration::from_secs(60);

/// Consecutive young deaths after which a starting daemon refuses to serve
/// until `a daemon restart`. `[daemon] crash_limit` overrides it; 0 turns
/// the breaker off.
pub const LIMIT: u32 = 3;

/// Who holds the daemon lock: what `run/daemon.pid` says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub pid: u32,
    /// Seconds since the Unix epoch at start.
    pub started: u64,
    /// The OS's own start time for `pid` (see [`crate::process_started`]),
    /// which tells this process from a later one given the same pid.
    pub token: Option<u64>,
    pub build: String,
}

impl Record {
    /// This process, as build `build`.
    pub fn current(build: &str) -> Self {
        let pid = std::process::id();
        Self {
            pid,
            started: now(),
            token: crate::process_started(pid),
            build: build.to_owned(),
        }
    }

    /// Whether `pid` is still the process this record describes. Without a
    /// start time to compare, only the pid can be.
    pub fn is_live(&self) -> bool {
        match (self.token, crate::process_started(self.pid)) {
            (Some(recorded), Some(now)) => recorded == now,
            (Some(_), None) => false,
            (None, _) => true,
        }
    }
}

/// One field a line: the pid first, so older readers still find it.
impl fmt::Display for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let token = self.token.map_or_else(|| "-".to_owned(), |t| t.to_string());
        writeln!(f, "{}\n{}\n{token}\n{}", self.pid, self.started, self.build)
    }
}

impl FromStr for Record {
    type Err = io::Error;

    fn from_str(text: &str) -> io::Result<Self> {
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "unreadable daemon record");
        let mut lines = text.lines().map(str::trim);
        let pid = lines.next().and_then(|l| l.parse().ok()).ok_or_else(bad)?;
        Ok(Self {
            pid,
            started: lines.next().and_then(|l| l.parse().ok()).unwrap_or(0),
            token: lines.next().and_then(|l| l.parse().ok()),
            build: lines.next().unwrap_or_default().to_owned(),
        })
    }
}

/// How a daemon's run ended, as the breaker counts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// It exited on its own: idle, drained, or asked to stop.
    Clean,
    /// It died holding the lock, after running for `lived`.
    Crashed { lived: Duration },
    /// A person asked for a fresh daemon (`a daemon restart`).
    Reset,
}

/// The crash-loop breaker: how many daemons in a row died within [`YOUNG`]
/// of starting. [`Breaker::on`] is its whole transition function, and only
/// the daemon holding the lock applies it, so the count has one writer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Breaker {
    young: u32,
}

impl Breaker {
    /// The breaker after a daemon ended `ended`: a clean exit or a reset
    /// ends any run of deaths, a young death extends it, and one that was
    /// healthy for [`YOUNG`] first is no loop.
    #[must_use]
    pub const fn on(self, ended: Ended) -> Self {
        let young = match ended {
            Ended::Crashed { lived } if lived.as_millis() < YOUNG.as_millis() => {
                self.young.saturating_add(1)
            }
            Ended::Clean | Ended::Reset | Ended::Crashed { .. } => 0,
        };
        Self { young }
    }

    /// Young deaths in a row.
    pub const fn young(self) -> u32 {
        self.young
    }

    /// Whether a starting daemon must refuse to serve, after `limit` young
    /// deaths in a row (0 for never). A refusing daemon records nothing, so
    /// its exit is no death and the breaker stays as it is until a clean
    /// exit or a reset.
    pub const fn tripped(self, limit: u32) -> bool {
        limit > 0 && self.young >= limit
    }

    /// The breaker as `home` has it.
    pub fn of(home: &Home) -> Self {
        let young = fs::read_to_string(breaker_file(home))
            .ok()
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0);
        Self { young }
    }

    fn store(self, home: &Home) -> io::Result<()> {
        if self.young == 0 {
            return match fs::remove_file(breaker_file(home)) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        fs::create_dir_all(home.run_dir())?;
        fs::write(breaker_file(home), format!("{}\n", self.young))
    }

    /// Applies `ended` to the breaker `home` has.
    fn note(home: &Home, ended: Ended) -> io::Result<Self> {
        let next = Self::of(home).on(ended);
        next.store(home)?;
        Ok(next)
    }
}

/// How the daemon's crashes stand.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Crashes {
    pub breaker: Breaker,
    /// The newest report.
    pub last: Option<PathBuf>,
}

impl Crashes {
    pub fn of(home: &Home) -> Self {
        Self {
            breaker: Breaker::of(home),
            last: reports(&home.crash_dir()).pop(),
        }
    }
}

/// Clears the breaker, for a user who asked for a fresh daemon.
pub fn reset(home: &Home) -> io::Result<()> {
    Breaker::note(home, Ended::Reset).map(drop)
}

/// Notes a clean exit in the daemon's record, `pid_file`: emptied, so no
/// one reaps it as a death, and the breaker cleared, so deaths before it and
/// after are not in a row.
pub fn exited(home: &Home, pid_file: &File) -> io::Result<()> {
    pid_file.set_len(0)?;
    Breaker::note(home, Ended::Clean).map(drop)
}

/// Notes a daemon that died holding the lock, if the record says one did:
/// a report (unless its panic hook wrote one), the breaker's count, and the
/// record emptied. Callers hold the daemon lock, so the record's daemon is
/// gone. Returns the dead daemon's record.
pub fn reap(home: &Home, pid_file: &mut File) -> io::Result<Option<Record>> {
    use std::io::{Read, Seek};
    let mut text = String::new();
    pid_file.seek(io::SeekFrom::Start(0))?;
    pid_file.read_to_string(&mut text)?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    pid_file.set_len(0)?;
    // The caller writes its own record next, from the start.
    pid_file.rewind()?;
    let Ok(dead) = text.parse::<Record>() else {
        return Ok(None);
    };
    let dir = home.crash_dir();
    let explained = reports(&dir)
        .iter()
        .filter_map(|p| report_key(p))
        .any(|key| {
            key & 0xffff_ffff == u128::from(dead.pid)
                && key >> 32 >= u128::from(dead.started) * 1000
        });
    if !explained {
        write_report(
            &dir,
            &Report {
                pid: dead.pid,
                started: Some(dead.started),
                unix_ms: millis(),
                build: Build::of(&dead.build),
                kind: Kind::UncleanExit,
                callsite: None,
                frames: Frames::default(),
            },
        )?;
    }
    let lived = Duration::from_secs(now().saturating_sub(dead.started));
    Breaker::note(home, Ended::Crashed { lived })?;
    Ok(Some(dead))
}

/// Installs the daemon's content-free panic hook. The default hook formats
/// arbitrary payloads and paths, so it must not run after this one.
pub fn install(dir: PathBuf, build: String) {
    let build = Build::of(&build);
    panic::set_hook(Box::new(move |info| {
        let _ = write_report(&dir, &describe(info, build));
    }));
}

/// Even an adversarial panic cannot enlarge a report or add text fields.
pub const MAX_REPORT: usize = 16 << 10;
const DEEP: usize = 128;

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Panic,
    UncleanExit,
}

#[derive(Serialize)]
struct Callsite {
    file: u64,
    line: u32,
    column: u32,
}

#[derive(Clone, Copy, Default, Serialize)]
struct Frame {
    image: u64,
    offset: u64,
}

struct Frames {
    frames: [Frame; DEEP],
    len: usize,
}
impl Default for Frames {
    fn default() -> Self {
        Self {
            frames: [Frame::default(); DEEP],
            len: 0,
        }
    }
}
impl Serialize for Frames {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.frames[..self.len].iter())
    }
}

#[derive(Serialize)]
struct Report {
    pid: u32,
    started: Option<u64>,
    unix_ms: u128,
    build: Build,
    kind: Kind,
    callsite: Option<Callsite>,
    frames: Frames,
}

fn describe(info: &PanicHookInfo<'_>, build: Build) -> Report {
    Report {
        pid: std::process::id(),
        started: None,
        unix_ms: millis(),
        build,
        kind: Kind::Panic,
        callsite: info.location().map(|at| Callsite {
            file: crate::fnv1a(at.file().as_bytes()),
            line: at.line(),
            column: at.column(),
        }),
        frames: stack(),
    }
}

#[cfg(not(unix))]
fn stack() -> Frames {
    Frames::default()
}

// No symbolization: only numeric image fingerprints and offsets cross the
// report boundary, even in debug builds. Build debug info resolves them offline.
#[cfg(unix)]
fn stack() -> Frames {
    use std::ffi::{CStr, c_int, c_void};
    unsafe extern "C" {
        fn _Unwind_Backtrace(
            frame: extern "C" fn(*mut c_void, *mut c_void) -> c_int,
            ips: *mut c_void,
        ) -> c_int;
        fn _Unwind_GetIP(cx: *mut c_void) -> usize;
    }
    extern "C" fn frame(cx: *mut c_void, frames: *mut c_void) -> c_int {
        // SAFETY: the same-thread unwinder borrows this fixed buffer until return.
        let frames = unsafe { &mut *frames.cast::<Frames>() };
        // SAFETY: cx is the unwinder's current frame context.
        let ip = unsafe { _Unwind_GetIP(cx) };
        let mut info = std::mem::MaybeUninit::<libc::Dl_info>::zeroed();
        // SAFETY: dladdr reads the address and fills the output buffer.
        let found = unsafe { libc::dladdr(ip as *const c_void, info.as_mut_ptr()) } != 0;
        // SAFETY: zeroed first, then filled by dladdr.
        let info = unsafe { info.assume_init() };
        let image = if found && !info.dli_fname.is_null() {
            // SAFETY: the loader owns this NUL-terminated name for this call.
            crate::fnv1a(unsafe { CStr::from_ptr(info.dli_fname) }.to_bytes())
        } else {
            0
        };
        frames.frames[frames.len] = Frame {
            image,
            offset: ip.saturating_sub(info.dli_fbase as usize) as u64,
        };
        frames.len += 1;
        if frames.len < DEEP { 0 } else { 5 }
    }
    let mut frames = Frames::default();
    // SAFETY: the buffer outlives this synchronous call; callback bounds its writes.
    unsafe { _Unwind_Backtrace(frame, (&raw mut frames).cast()) };
    frames
}

fn millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// The only writer accepts a closed report, never a string or panic payload.
fn write_report(dir: &Path, report: &Report) -> io::Result<PathBuf> {
    let mut bytes = serde_json::to_vec(report).map_err(io::Error::other)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_REPORT {
        return Err(io::ErrorKind::InvalidData.into());
    }
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}-{}.json", report.unix_ms, report.pid));
    File::create(&path)?.write_all(&bytes)?;
    let reports = reports(dir);
    for old in &reports[..reports.len().saturating_sub(KEEP)] {
        let _ = fs::remove_file(old);
    }
    Ok(path)
}

/// Reports in `dir`, oldest first.
pub fn reports(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut reports: Vec<_> = entries
        .filter_map(|e| Some(e.ok()?.path()))
        .filter_map(|p| Some((report_key(&p)?, p)))
        .collect();
    reports.sort();
    reports.into_iter().map(|(_, p)| p).collect()
}

/// A report's time in Unix milliseconds and the pid it is about, as
/// `ms << 32 | pid`: ordered as the pair is.
fn report_key(path: &Path) -> Option<u128> {
    let stem = path.file_name()?.to_str()?.strip_suffix(".json")?;
    let (ms, pid) = stem.split_once('-')?;
    let (ms, pid): (u128, u32) = (ms.parse().ok()?, pid.parse().ok()?);
    Some(ms << 32 | u128::from(pid))
}

fn breaker_file(home: &Home) -> PathBuf {
    home.run_dir().join("crashes")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(name: &str) -> Home {
        let root = std::env::temp_dir().join(format!("a-crash-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("run")).unwrap();
        Home::from(root)
    }

    /// Reaps `record` as a daemon holding the lock would, without taking
    /// it: a process the other tests fork holds this process's open files,
    /// and so their locks, until it execs.
    fn died(home: &Home, record: &Record) -> Option<Record> {
        fs::write(home.daemon_pid_file(), record.to_string()).unwrap();
        let mut file = File::options()
            .read(true)
            .write(true)
            .open(home.daemon_pid_file())
            .unwrap();
        reap(home, &mut file).unwrap()
    }

    #[test]
    fn a_record_reads_back_and_knows_its_process() {
        let me = Record::current("b1");
        assert_eq!(me.to_string().parse::<Record>().unwrap(), me);
        assert!(me.is_live());
        assert_eq!("42".parse::<Record>().unwrap().pid, 42);
        assert!("".parse::<Record>().is_err());
        #[cfg(any(target_os = "linux", target_vendor = "apple"))]
        {
            let reused = Record {
                token: me.token.map(|t| t + 1),
                ..me
            };
            assert!(!reused.is_live(), "another process under the same pid");
        }
    }

    #[test]
    fn deaths_soon_after_start_trip_the_breaker() {
        let home = home("breaker");
        let young = Record {
            pid: 999_999,
            started: now(),
            token: None,
            build: "b1".into(),
        };
        for n in 1..=LIMIT {
            assert!(!Breaker::of(&home).tripped(LIMIT));
            assert_eq!(died(&home, &young).as_ref(), Some(&young));
            assert_eq!(Breaker::of(&home).young(), n);
        }
        let crashes = Crashes::of(&home);
        assert!(crashes.breaker.tripped(LIMIT) && !crashes.breaker.tripped(0));
        let report = fs::read_to_string(crashes.last.unwrap()).unwrap();
        let report: serde_json::Value = serde_json::from_str(&report).unwrap();
        assert_eq!(report["pid"], 999999);
        assert_eq!(report["kind"], "unclean_exit");
        assert!(!report.to_string().contains("b1"));
        assert_eq!(fs::read_to_string(home.daemon_pid_file()).unwrap(), "");

        // One that ran a while is not a loop.
        let old = Record {
            started: now() - YOUNG.as_secs() * 2,
            ..young.clone()
        };
        died(&home, &old);
        assert_eq!(Breaker::of(&home).young(), 0);
        died(&home, &young);
        reset(&home).unwrap();
        assert_eq!(Breaker::of(&home).young(), 0);
        fs::remove_dir_all(home.crash_dir().parent().unwrap()).unwrap();
    }

    /// Deaths with a clean exit between them are not in a row.
    #[test]
    fn a_clean_exit_resets_the_breaker() {
        let home = home("clean");
        let young = Record {
            pid: 999_998,
            started: now(),
            token: None,
            build: "b1".into(),
        };
        for _ in 1..LIMIT {
            died(&home, &young);
        }
        let record = File::options()
            .write(true)
            .open(home.daemon_pid_file())
            .unwrap();
        exited(&home, &record).unwrap();
        died(&home, &young);
        let breaker = Breaker::of(&home);
        fs::remove_dir_all(home.crash_dir().parent().unwrap()).unwrap();
        assert_eq!(breaker.young(), 1);
        assert!(!breaker.tripped(LIMIT));
    }

    #[test]
    fn the_breaker_moves_as_its_table_says() {
        let young = Ended::Crashed {
            lived: YOUNG.saturating_sub(Duration::from_millis(1)),
        };
        let healthy = Ended::Crashed { lived: YOUNG };
        let at = |young| Breaker { young };
        // (from, event, to)
        let table = [
            (0, Ended::Clean, 0),
            (0, Ended::Reset, 0),
            (0, young, 1),
            (0, healthy, 0),
            (2, young, 3),
            (2, Ended::Clean, 0),
            (2, Ended::Reset, 0),
            (2, healthy, 0),
            (LIMIT, young, LIMIT + 1),
            (u32::MAX, young, u32::MAX),
        ];
        for (from, event, to) in table {
            assert_eq!(at(from).on(event), at(to), "{from} on {event:?}");
        }
        assert!(!at(LIMIT - 1).tripped(LIMIT) && at(LIMIT).tripped(LIMIT));
        assert!(!at(u32::MAX).tripped(0), "a limit of 0 is no breaker");
    }

    /// However long young deaths go on, the breaker trips by the limit and
    /// stays tripped: a daemon that refuses to serve writes no record, so
    /// the next one has no death to count and nothing to clear. Only a
    /// clean exit or a reset closes it.
    #[test]
    fn a_crash_loop_cannot_retry_forever() {
        let young = Ended::Crashed {
            lived: Duration::ZERO,
        };
        for limit in 1..=5 {
            let mut breaker = Breaker::default();
            let tripped_at = (1..=limit * 10).find(|_| {
                breaker = breaker.on(young);
                breaker.tripped(limit)
            });
            assert_eq!(tripped_at, Some(limit));
            for _ in 0..100 {
                breaker = breaker.on(young);
                assert!(breaker.tripped(limit));
            }
            assert!(!breaker.on(Ended::Reset).tripped(limit));
        }

        let home = home("loop");
        let dead = Record {
            pid: 999_997,
            started: now(),
            token: None,
            build: "b1".into(),
        };
        for _ in 0..LIMIT {
            died(&home, &dead);
        }
        assert!(Breaker::of(&home).tripped(LIMIT));
        // Refusing daemons, each reaping what the last left: nothing.
        let mut file = File::options()
            .read(true)
            .write(true)
            .open(home.daemon_pid_file())
            .unwrap();
        for _ in 0..10 {
            assert_eq!(reap(&home, &mut file).unwrap(), None);
            assert!(Breaker::of(&home).tripped(LIMIT));
        }
        reset(&home).unwrap();
        let breaker = Breaker::of(&home);
        fs::remove_dir_all(home.crash_dir().parent().unwrap()).unwrap();
        assert_eq!(breaker, Breaker::default());
    }

    /// A daemon that reaps its predecessor records itself in its place.
    #[test]
    fn the_reaper_writes_its_record_from_the_start() {
        let home = home("reaper");
        let dead = Record::current("b1");
        fs::write(home.daemon_pid_file(), dead.to_string()).unwrap();
        let mut file = File::options()
            .read(true)
            .write(true)
            .open(home.daemon_pid_file())
            .unwrap();
        assert_eq!(reap(&home, &mut file).unwrap(), Some(dead));
        let me = Record::current("b2");
        write!(file, "{me}").unwrap();
        let text = fs::read_to_string(home.daemon_pid_file()).unwrap();
        fs::remove_dir_all(home.crash_dir().parent().unwrap()).unwrap();
        assert_eq!(text.parse::<Record>().unwrap(), me);
    }

    #[test]
    fn a_panic_report_is_not_written_twice() {
        let home = home("panic");
        let pid = 999_998;
        let started = now();
        let mut report = Report {
            pid,
            started: None,
            unix_ms: millis(),
            build: Build::of("1.2.3+0123456789abcdef"),
            kind: Kind::Panic,
            callsite: None,
            frames: Frames::default(),
        };
        write_report(&home.crash_dir(), &report).unwrap();
        died(
            &home,
            &Record {
                pid,
                started,
                token: None,
                build: "b1".into(),
            },
        );
        let reports = reports(&home.crash_dir());
        assert_eq!(reports.len(), 1);
        let text = fs::read_to_string(&reports[0]).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap()["kind"],
            "panic"
        );
        for n in 0..KEEP + 3 {
            report.pid = u32::try_from(n).unwrap();
            write_report(&home.crash_dir(), &report).unwrap();
        }
        assert_eq!(super::reports(&home.crash_dir()).len(), KEEP);
        fs::remove_dir_all(home.crash_dir().parent().unwrap()).unwrap();
    }

    #[test]
    fn every_report_shape_fits_the_file_cap() {
        let home = home("cap");
        let report = Report {
            pid: u32::MAX,
            started: Some(u64::MAX),
            unix_ms: u128::MAX >> 32,
            build: Build::of("4294967295.4294967295.4294967295+ffffffffffffffff"),
            kind: Kind::Panic,
            callsite: Some(Callsite {
                file: u64::MAX,
                line: u32::MAX,
                column: u32::MAX,
            }),
            frames: Frames {
                frames: [Frame {
                    image: u64::MAX,
                    offset: u64::MAX,
                }; DEEP],
                len: DEEP,
            },
        };
        let path = write_report(&home.crash_dir(), &report).unwrap();
        let bytes = fs::read(path).unwrap();
        assert!(bytes.len() <= MAX_REPORT);
        assert!(serde_json::from_slice::<serde_json::Value>(&bytes).is_ok());
        fs::remove_dir_all(home.crash_dir().parent().unwrap()).unwrap();
    }

    /// Panics with the hook installed when `A_CRASH_TEST_DIR` is set; run
    /// by the next test as a process of its own, since the panic may not
    /// unwind (the dev profile's Cranelift code aborts instead).
    #[test]
    fn panicking_child() {
        if let Some(dir) = std::env::var_os("A_CRASH_TEST_DIR") {
            install(dir.into(), "secret-build-text".into());
            let _ = std::thread::Builder::new()
                .name("secret-thread-text".into())
                .spawn(|| panic::panic_any("secret-payload-text".repeat(100_000)))
                .unwrap()
                .join();
        }
    }

    #[test]
    fn panic_content_cannot_enter_a_bounded_report() {
        let home = home("hook");
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash::tests::panicking_child", "--nocapture"])
            .env("A_CRASH_TEST_DIR", home.crash_dir())
            .output()
            .unwrap();
        let reports = reports(&home.crash_dir());
        let report = reports.first().map_or_else(
            || panic!("no report: {child:?}"),
            |r| fs::read_to_string(r).unwrap(),
        );
        fs::remove_dir_all(home.crash_dir().parent().unwrap()).unwrap();
        assert_eq!(reports.len(), 1);
        assert!(report.len() <= MAX_REPORT);
        for forbidden in ["secret", "crash.rs", "thread", "payload", "/Users/", "\\"] {
            assert!(!report.contains(forbidden), "{forbidden} in {report}");
        }
        let value: serde_json::Value = serde_json::from_str(&report).unwrap();
        assert_eq!(value["kind"], "panic");
        assert!(value["build"]["version"].is_null());
        assert!(value["build"]["fingerprint"].is_null());
        assert!(value["callsite"]["file"].is_u64());
        assert!(value["callsite"]["line"].is_u64());
        for frame in value["frames"].as_array().unwrap() {
            assert!(frame["image"].is_u64() && frame["offset"].is_u64());
        }
    }
}
