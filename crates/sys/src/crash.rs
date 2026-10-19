//! What a dying daemon leaves behind: a report per crash under `crash/`,
//! and a breaker that stops a daemon that keeps dying soon after it starts
//! from serving again.
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

/// How the daemon's crashes stand.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Crashes {
    /// Deaths in a row, each within [`YOUNG`] of starting.
    pub young: u32,
    /// The newest report.
    pub last: Option<PathBuf>,
}

impl Crashes {
    pub fn of(home: &Home) -> Self {
        Self {
            young: fs::read_to_string(breaker_file(home))
                .ok()
                .and_then(|t| t.trim().parse().ok())
                .unwrap_or(0),
            last: reports(&home.crash_dir()).pop(),
        }
    }

    /// Whether a daemon should refuse to serve, after `limit` young deaths
    /// in a row (0 for never).
    pub const fn tripped(&self, limit: u32) -> bool {
        limit > 0 && self.young >= limit
    }
}

/// Clears the breaker, for a user who asked for a fresh daemon.
pub fn reset(home: &Home) -> io::Result<()> {
    match fs::remove_file(breaker_file(home)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Notes a clean exit in the daemon's record, `pid_file`: emptied, so no
/// one reaps it as a death, and the breaker cleared, so deaths before it and
/// after are not in a row.
pub fn exited(home: &Home, pid_file: &File) -> io::Result<()> {
    pid_file.set_len(0)?;
    reset(home)
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
            dead.pid,
            &format!(
                "a {} daemon (pid {}) stopped without shutting down\n\
                 It was killed, or crashed in a way no panic hook sees: a signal, an \
                 abort, running out of memory. Its log may say more.\n\
                 started: {}\nnoticed: {}\n",
                dead.build,
                dead.pid,
                dead.started,
                now()
            ),
        )?;
    }
    let young = now().saturating_sub(dead.started) < YOUNG.as_secs();
    let count = if young {
        Crashes::of(home).young + 1
    } else {
        0
    };
    fs::create_dir_all(home.run_dir())?;
    fs::write(breaker_file(home), format!("{count}\n"))?;
    Ok(Some(dead))
}

/// Writes a report on every panic to `dir`, then runs the hook that was
/// there (which prints to stderr, the daemon's log). A panic a thread
/// recovers from is reported too: it is a bug either way.
pub fn install(dir: PathBuf, build: String) {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = write_report(&dir, std::process::id(), &describe(info, &build));
        previous(info);
    }));
}

fn describe(info: &PanicHookInfo<'_>, build: &str) -> String {
    let thread = std::thread::current();
    let message = info
        .payload()
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(not a string)");
    let at = info
        .location()
        .map_or_else(String::new, ToString::to_string);
    format!(
        "a {build} daemon (pid {}) panicked\nthread: {}\nat: {at}\nmessage: {message}\ntime: {}\n\n{}\n",
        std::process::id(),
        thread.name().unwrap_or("unnamed"),
        now(),
        stack()
    )
}

/// This thread's stack. A build with a symbolizing std names its frames.
#[cfg(any(debug_assertions, not(unix)))]
fn stack() -> String {
    std::backtrace::Backtrace::force_capture().to_string()
}

/// This thread's stack, a frame a line as the image holding its return
/// address and the offset into it, which the build's debug info names
/// offline (`atos -o a.dSYM/... -l 0x100000000 <0x100000000 + offset>`,
/// `addr2line -e a <offset>`). A release std carries no symbolizer, and
/// std's [`Backtrace`](std::backtrace::Backtrace) would link its demangler
/// for names it never resolves, and print addresses the image's load slide
/// leaves meaningless.
#[cfg(all(unix, not(debug_assertions)))]
fn stack() -> String {
    use std::ffi::{CStr, c_int, c_void};
    use std::fmt::Write as _;

    /// Frames past it are left out: a crash report is read, not replayed.
    const DEEP: usize = 128;

    unsafe extern "C" {
        fn _Unwind_Backtrace(
            frame: extern "C" fn(*mut c_void, *mut c_void) -> c_int,
            ips: *mut c_void,
        ) -> c_int;
        fn _Unwind_GetIP(cx: *mut c_void) -> usize;
    }

    extern "C" fn frame(cx: *mut c_void, ips: *mut c_void) -> c_int {
        // SAFETY: `ips` is the vector `stack` hands the unwinder, which
        // calls this on the same thread while `stack` waits.
        let ips = unsafe { &mut *ips.cast::<Vec<usize>>() };
        // SAFETY: `cx` is the unwinder's context for the frame it is at.
        ips.push(unsafe { _Unwind_GetIP(cx) });
        // _URC_NO_REASON goes on; _URC_END_OF_STACK (5) stops.
        if ips.len() < DEEP { 0 } else { 5 }
    }

    let mut ips: Vec<usize> = Vec::with_capacity(DEEP);
    // SAFETY: `frame` only pushes to `ips`, which outlives the call.
    unsafe { _Unwind_Backtrace(frame, (&raw mut ips).cast()) };
    let mut out = String::new();
    for (n, ip) in ips.into_iter().enumerate() {
        let mut info = std::mem::MaybeUninit::<libc::Dl_info>::zeroed();
        // SAFETY: dladdr only reads the address and fills `info`.
        let found = unsafe { libc::dladdr(ip as *const c_void, info.as_mut_ptr()) } != 0;
        // SAFETY: zeroed, then filled by dladdr; a null name is checked.
        let info = unsafe { info.assume_init() };
        let image = (found && !info.dli_fname.is_null())
            // SAFETY: dladdr's file name is a C string the loader owns.
            .then(|| unsafe { CStr::from_ptr(info.dli_fname) }.to_string_lossy())
            .map(|path| path.rsplit('/').next().unwrap_or_default().to_owned());
        let _ = match image {
            Some(image) => writeln!(out, "{n:4}: {image} +{:#x}", ip - info.dli_fbase as usize),
            None => writeln!(out, "{n:4}: {ip:#x}"),
        };
    }
    out
}

/// Writes `text` as a new report named `<unix ms>-<pid>.txt`, keeping the
/// newest [`KEEP`].
fn write_report(dir: &Path, pid: u32, text: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let path = dir.join(format!("{ms}-{pid}.txt"));
    File::create(&path)?.write_all(text.as_bytes())?;
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
    let stem = path.file_name()?.to_str()?.strip_suffix(".txt")?;
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
            assert!(!Crashes::of(&home).tripped(LIMIT));
            assert_eq!(died(&home, &young).as_ref(), Some(&young));
            assert_eq!(Crashes::of(&home).young, n);
        }
        let crashes = Crashes::of(&home);
        assert!(crashes.tripped(LIMIT) && !crashes.tripped(0));
        let report = fs::read_to_string(crashes.last.unwrap()).unwrap();
        assert!(report.contains("pid 999999") && report.contains("without shutting down"));
        assert_eq!(fs::read_to_string(home.daemon_pid_file()).unwrap(), "");

        // One that ran a while is not a loop.
        let old = Record {
            started: now() - YOUNG.as_secs() * 2,
            ..young.clone()
        };
        died(&home, &old);
        assert_eq!(Crashes::of(&home).young, 0);
        died(&home, &young);
        reset(&home).unwrap();
        assert_eq!(Crashes::of(&home).young, 0);
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
        let crashes = Crashes::of(&home);
        fs::remove_dir_all(home.crash_dir().parent().unwrap()).unwrap();
        assert_eq!(crashes.young, 1);
        assert!(!crashes.tripped(LIMIT));
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
        write_report(&home.crash_dir(), pid, "panicked\n").unwrap();
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
        assert_eq!(fs::read_to_string(&reports[0]).unwrap(), "panicked\n");
        for n in 0..KEEP + 3 {
            write_report(&home.crash_dir(), u32::try_from(n).unwrap(), "x").unwrap();
        }
        assert_eq!(super::reports(&home.crash_dir()).len(), KEEP);
        fs::remove_dir_all(home.crash_dir().parent().unwrap()).unwrap();
    }

    /// Panics with the hook installed when `A_CRASH_TEST_DIR` is set; run
    /// by the next test as a process of its own, since the panic may not
    /// unwind (the dev profile's Cranelift code aborts instead).
    #[test]
    fn panicking_child() {
        if let Some(dir) = std::env::var_os("A_CRASH_TEST_DIR") {
            install(dir.into(), "b1".into());
            let _ = std::thread::Builder::new()
                .name("doomed".into())
                .spawn(|| panic!("boom"))
                .unwrap()
                .join();
        }
    }

    #[test]
    fn a_panic_leaves_a_report() {
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
        for want in [
            "a b1 daemon",
            "panicked",
            "thread: doomed",
            "message: boom",
            "crash.rs",
        ] {
            assert!(report.contains(want), "{want} in {report}");
        }
    }
}
