//! Immutable installed builds of `a` under the executable data directory.
//! Installation happens once per content identity; ordinary starts resolve an
//! existing image without copying, hashing, flushing or publishing references.
//! The daemon runs `runtime/current`, so the newest installed build wins.

use std::fs::{self, File};
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::fnv1a;

mod image;

const EXE: &str = if cfg!(windows) { "a.exe" } else { "a" };

/// The version every build id starts with.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The staging area of one home.
#[derive(Clone, Debug)]
pub struct Runtime {
    dir: PathBuf,
}

/// A staged build. Its id, `<version>+<content hash>`, is the build identity
/// clients and daemons compare in their hello.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Staged {
    pub id: String,
    /// The staged copy, or the original when an older build was pruned
    /// right away.
    pub exe: PathBuf,
}

impl Runtime {
    pub const fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// What the daemon runs: `current/a`, resolved by `exec`.
    pub fn current_exe(&self) -> PathBuf {
        #[cfg(unix)]
        {
            self.dir.join("current").join(EXE)
        }
        #[cfg(windows)]
        {
            self.dir.join(self.current().unwrap_or_default()).join(EXE)
        }
    }

    /// The id `current` points at.
    pub fn current(&self) -> Option<String> {
        let target = read_reference(&self.dir.join("current"))?;
        target.into_os_string().into_string().ok()
    }

    /// Installs `exe` once (clone/copy, verify, then publish if it is newer)
    /// and keeps the two newest builds. Hashes the binary only the first time
    /// a given file is seen: a stamp keyed by its path and metadata remembers
    /// the id. A build already outranked (staged once, then pruned) is not
    /// copied again, so an older `a` that keeps being run
    /// costs a lookup, not a copy.
    pub fn stage(&self, exe: &Path) -> io::Result<Staged> {
        let exe = fs::canonicalize(exe)?;
        let known = self.id_in_runtime(&exe).or_else(|| self.stamped(&exe));
        let id = match known {
            Some(id) if self.settled(&id) => id,
            _ => {
                let _lock = self.lock()?;
                // Another installer may have completed while this one waited.
                let known = self.id_in_runtime(&exe).or_else(|| self.stamped(&exe));
                let id = match known.filter(|id| self.is_staged(id)) {
                    Some(id) => {
                        if !self.dir.join(&id).join("installed").is_file() {
                            image::flush(&self.dir.join(&id).join(EXE))?;
                            self.record_identity(&id)?;
                        }
                        id
                    }
                    None => self.copy_in(&exe)?,
                };
                self.promote(&id)?;
                id
            }
        };
        let exe = Some(self.dir.join(&id).join(EXE))
            .filter(|staged| staged.is_file())
            .unwrap_or(exe);
        Ok(Staged { id, exe })
    }

    /// Declares, as build `id` itself runs, the handover format it speaks
    /// ([`crate::handover::VERSION`]), in `runtime/<id>/handover`; a
    /// declaration already there is left as it is.
    pub fn declare_handover(&self, id: &str) -> io::Result<()> {
        let dir = self.dir.join(id);
        if !dir.is_dir() {
            return Ok(());
        }
        match File::create_new(dir.join("handover")) {
            Ok(mut file) => {
                io::Write::write_all(&mut file, crate::handover::VERSION.to_string().as_bytes())
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// The handover format build `id` declared, if it did.
    pub fn handover(&self, id: &str) -> Option<u32> {
        fs::read_to_string(self.dir.join(id).join("handover"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// The id of an exe that already is `runtime/<id>/a`.
    fn id_in_runtime(&self, exe: &Path) -> Option<String> {
        let dir = exe.parent()?;
        if exe.file_name()? != EXE || dir.parent()? != fs::canonicalize(&self.dir).ok()? {
            return None;
        }
        dir.file_name()?.to_str().map(str::to_owned)
    }

    fn stamp(&self, exe: &Path) -> io::Result<PathBuf> {
        let mut key = exe.as_os_str().as_encoded_bytes().to_vec();
        image::stamp(exe, &mut key)?;
        Ok(self
            .dir
            .join("stamps")
            .join(format!("{:016x}", fnv1a(&key))))
    }

    /// The id a stamp remembers for `exe`, staged or since pruned.
    fn stamped(&self, exe: &Path) -> Option<String> {
        let id = read_reference(&self.stamp(exe).ok()?)?.into_os_string();
        id.into_string().ok()
    }

    fn is_staged(&self, id: &str) -> bool {
        self.dir.join(id).join(EXE).is_file()
    }

    /// A completed install must not acquire the installer lock, including
    /// clients using the retained runner-up or a since-pruned older build.
    fn settled(&self, id: &str) -> bool {
        let Some(current) = self.current().filter(|id| self.is_staged(id)) else {
            return false;
        };
        self.order(id).is_some_and(|order| {
            current == id || self.order(&current).is_some_and(|current| order <= current)
        })
    }

    /// Copies `exe` to `runtime/<id>/a` through a temporary file, checks the
    /// copy's hash, and renames it into place.
    fn copy_in(&self, exe: &Path) -> io::Result<String> {
        // A stamp only saves hashing next time; the copy is verified by
        // content, so a source whose metadata cannot be read or keeps
        // changing is still installed, just not remembered.
        let stamp = self.stamp(exe).ok();
        let hash = content_hash(File::open(exe)?)?;
        let id = format!("{VERSION}+{hash:016x}");
        let dir = self.dir.join(&id);
        let target = dir.join(EXE);
        let outranked = self.order(&id).is_some_and(|order| {
            self.current().is_some_and(|current| {
                self.is_staged(&current)
                    && self.order(&current).is_some_and(|current| order <= current)
            })
        });
        if !target.is_file() && !outranked {
            fs::create_dir_all(&dir)?;
            let tmp = dir.join(format!(".a.{}", std::process::id()));
            let _ = image::discard(&tmp);
            let installed = (|| {
                image::copy(exe, &tmp)?;
                let file = File::open(&tmp)?;
                if content_hash(&file)? != hash {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the executable changed during installation",
                    ));
                }
                image::flush(&tmp)?;
                image::publish(&tmp, &target)
            })();
            if installed.is_err() {
                let _ = image::discard(&tmp);
            }
            installed?;
        }
        if target.is_file() && !dir.join("installed").is_file() {
            self.record_identity(&id)?;
        }
        if let Some(stamp) = stamp.filter(|stamp| self.stamp(exe).ok().as_ref() == Some(stamp)) {
            let _ = crate::fs::private_dir(stamp.parent().unwrap_or(&self.dir))
                .and_then(|()| replace_reference(&id, &stamp));
        }
        Ok(id)
    }

    /// Points `current` at `id` if it is newer than what `current` names,
    /// never older, then prunes.
    fn promote(&self, id: &str) -> io::Result<()> {
        let newer = match self
            .current()
            .filter(|current| self.is_staged(current))
            .and_then(|current| self.order(&current))
        {
            Some(current) => self.order(id).is_some_and(|order| order > current),
            None => true,
        };
        if newer {
            replace_reference(id, &self.dir.join("current"))?;
        }
        self.prune()
    }

    /// Keeps two executable images and the newest older receipts, so the
    /// same content is never installed again just because its source path
    /// changes. Runs under the installer lock: any temporary name left here
    /// belongs to an installer that crashed.
    fn prune(&self) -> io::Result<()> {
        /// Enough for the few older installs one machine may keep running.
        const RECEIPTS: usize = 8;
        image::collect(&self.dir);
        let current = self.current();
        let mut builds = Vec::new();
        for name in fs::read_dir(&self.dir)?.filter_map(|e| e.ok()?.file_name().into_string().ok())
        {
            if name.starts_with(".link.") {
                let _ = image::discard(&self.dir.join(&name));
                continue;
            }
            if !name.contains('+') || Some(&name) == current.as_ref() {
                continue;
            }
            sweep(&self.dir.join(&name));
            match self.order(&name) {
                Some(order) => builds.push((order, name)),
                None if !self.is_staged(&name) => {
                    let _ = fs::remove_dir_all(self.dir.join(&name));
                }
                None => {}
            }
        }
        if let Some(current) = &current {
            sweep(&self.dir.join(current));
        }
        builds.sort_unstable_by(|a, b| b.cmp(a));
        let (mut image_kept, mut receipts) = (false, 0);
        for (_, id) in builds {
            if self.is_staged(&id) {
                if !image_kept {
                    image_kept = true;
                    continue;
                }
                match image::retire(&self.dir.join(&id).join(EXE)) {
                    // A scanner or foreign handle can temporarily deny deletion;
                    // installation must still let the newer image run.
                    #[cfg(windows)]
                    Err(e) if matches!(e.raw_os_error(), Some(5 | 32)) => {}
                    result => result?,
                }
            }
            if receipts < RECEIPTS {
                receipts += 1;
            } else {
                // Windows reclaims a retired image still mapped here later.
                let _ = fs::remove_dir_all(self.dir.join(&id));
            }
        }
        let Ok(stamps) = fs::read_dir(self.dir.join("stamps")) else {
            return Ok(());
        };
        for stamp in stamps.filter_map(Result::ok).map(|e| e.path()) {
            let dangling = stamp
                .file_name()
                .and_then(|name| name.to_str())
                .is_none_or(|name| name.starts_with(".link."))
                || read_reference(&stamp).is_none_or(|id| !self.dir.join(id).is_dir());
            if dangling {
                let _ = image::discard(&stamp);
            }
        }
        Ok(())
    }

    fn record_identity(&self, id: &str) -> io::Result<()> {
        let dir = self.dir.join(id);
        crate::fs::private_dir(&dir)?;
        let tmp = dir.join(format!(".installed.{}", std::process::id()));
        let _ = image::discard(&tmp);
        let file = File::options().write(true).create_new(true).open(&tmp)?;
        file.set_modified(fs::metadata(dir.join(EXE))?.modified()?)?;
        image::order(&file)?;
        image::publish(&tmp, &dir.join("installed"))
    }

    /// Installation receipts retain build order even after image reclamation.
    fn order(&self, id: &str) -> Option<Order> {
        let built = fs::metadata(self.dir.join(id).join("installed"))
            .ok()?
            .modified()
            .ok()?;
        let version = id.split_once('+').map_or(id, |(v, _)| v);
        Some(Order {
            version: version
                .split('.')
                .map(|part| part.parse().unwrap_or(0))
                .collect(),
            built,
        })
    }

    /// Serializes stagers so a prune never races a copy or a repoint.
    fn lock(&self) -> io::Result<File> {
        crate::fs::private_dir(&self.dir)?;
        let file = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dir.join(".lock"))?;
        crate::fs::lock(&file)?;
        Ok(file)
    }
}

/// Removes the temporary names a crashed installer left in a build.
fn sweep(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for name in entries.filter_map(|e| e.ok()?.file_name().into_string().ok()) {
        if name.starts_with(".a.") || name.starts_with(".installed.") {
            let _ = image::discard(&dir.join(name));
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Order {
    version: Vec<u64>,
    built: SystemTime,
}

/// Atomically publishes a build id: a relative symlink on Unix, a small
/// reference file on Windows, where symlinks would need elevated rights.
fn replace_reference(target: &str, link: &Path) -> io::Result<()> {
    let tmp = link.with_file_name(format!(".link.{}", std::process::id()));
    let _ = image::discard(&tmp);
    #[cfg(unix)]
    symlink(target, &tmp)?;
    #[cfg(windows)]
    {
        fs::write(&tmp, target)?;
        File::options().write(true).open(&tmp)?.sync_all()?;
    }
    // A lost reference is recomputed: `current` from the installed builds,
    // a stamp by hashing its source once more.
    crate::fs::replace(&tmp, link)
}

fn read_reference(path: &Path) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        fs::read_link(path).ok()
    }
    #[cfg(windows)]
    {
        fs::read_to_string(path)
            .ok()
            .filter(|id| !id.is_empty() && !id.contains(['/', '\\', ':', '\n', '\r']))
            .map(PathBuf::from)
    }
}

/// FNV-1a over 64-bit words with a rotate, so a multi-megabyte binary hashes
/// in milliseconds even unoptimized. Detects corrupt copies and names builds;
/// not a defence against an attacker, who could replace `runtime/` anyway.
///
/// Read in blocks: a buffer the size of the binary, once freed, stays in
/// the RSS of a client that just staged it (8 MB of a 15 MB terminal
/// client on macOS).
fn content_hash(mut from: impl Read) -> io::Result<u64> {
    /// A multiple of 8, so only the last block has a tail.
    const BLOCK: usize = 64 * 1024;
    let mut block = Vec::with_capacity(BLOCK);
    let (mut h, mut len) = (0xcbf2_9ce4_8422_2325_u64, 0);
    loop {
        block.clear();
        (&mut from).take(BLOCK as u64).read_to_end(&mut block)?;
        len += block.len() as u64;
        let (words, tail) = block.as_chunks::<8>();
        h = words.iter().fold(h, |h, w| {
            (h ^ u64::from_le_bytes(*w))
                .wrapping_mul(0x0100_0000_01b3)
                .rotate_left(29)
        });
        if block.len() < BLOCK {
            return Ok(fnv1a(tail).wrapping_add(h) ^ len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("a-runtime-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("bin")).unwrap();
        dir
    }

    /// A fake build: `bytes`, built `age` seconds after the epoch.
    fn build(dir: &Path, name: &str, bytes: &[u8], age: u64) -> PathBuf {
        let exe = dir.join("bin").join(name);
        fs::write(&exe, bytes).unwrap();
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + age);
        File::options()
            .write(true)
            .open(&exe)
            .unwrap()
            .set_modified(time)
            .unwrap();
        exe
    }

    fn builds(runtime: &Path) -> Vec<String> {
        let mut ids: Vec<String> = fs::read_dir(runtime)
            .unwrap()
            .filter_map(|e| e.unwrap().file_name().into_string().ok())
            .filter(|n| n.contains('+') && runtime.join(n).join(EXE).is_file())
            .collect();
        ids.sort();
        ids
    }

    #[test]
    fn stages_newest_keeps_two_and_never_downgrades() {
        let dir = tmp("stage");
        let runtime = Runtime::new(dir.join("runtime"));
        let old = runtime.stage(&build(&dir, "old", b"old build", 1)).unwrap();
        assert_eq!(fs::read(&old.exe).unwrap(), b"old build");
        assert_eq!(runtime.current(), Some(old.id.clone()));

        let mid = runtime.stage(&build(&dir, "mid", b"mid build", 2)).unwrap();
        let new = runtime.stage(&build(&dir, "new", b"new build", 3)).unwrap();
        assert_eq!(runtime.current(), Some(new.id.clone()));
        assert_eq!(fs::read(runtime.current_exe()).unwrap(), b"new build");
        let mut kept = vec![mid.id.clone(), new.id.clone()];
        kept.sort();
        assert_eq!(builds(&dir.join("runtime")), kept);

        // An older build stages but does not take over, and is kept as the
        // runner-up only if it is newer than the previous one.
        let again = runtime
            .stage(&build(&dir, "old2", b"older build", 0))
            .unwrap();
        assert_eq!(runtime.current(), Some(new.id.clone()));
        assert!(
            !again
                .exe
                .starts_with(fs::canonicalize(&dir).unwrap().join("runtime"))
        );
        assert_eq!(builds(&dir.join("runtime")), kept);
        // Running it again finds it outranked by its stamp: no copy, which
        // a read-only runtime would refuse.
        let runtime_dir = dir.join("runtime");
        #[cfg(unix)]
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o555)).unwrap();
        let restaged = runtime.stage(&dir.join("bin/old2"));
        #[cfg(unix)]
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o755)).unwrap();
        #[cfg(windows)]
        let _ = runtime_dir;
        assert_eq!(restaged.unwrap(), again);

        // A known file is recognised by its stamp; the image is never rewritten.
        let installed = fs::metadata(&new.exe).unwrap().modified().unwrap();
        assert_eq!(runtime.stage(&dir.join("bin/new")).unwrap().id, new.id);
        assert_eq!(
            fs::metadata(&new.exe).unwrap().modified().unwrap(),
            installed
        );
        // Running the staged copy itself needs no stamp.
        assert_eq!(runtime.stage(&mid.exe).unwrap().id, mid.id);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_higher_version_wins_over_a_later_build() {
        let dir = tmp("version");
        let runtime = Runtime::new(dir.join("runtime"));
        let staged = runtime.stage(&build(&dir, "a", b"x", 5)).unwrap();
        // A hand-made build of a higher version, built earlier.
        let higher = "99.0.0+0000000000000001";
        fs::create_dir_all(dir.join("runtime").join(higher)).unwrap();
        fs::copy(&staged.exe, dir.join("runtime").join(higher).join(EXE)).unwrap();
        File::options()
            .write(true)
            .open(dir.join("runtime").join(higher).join(EXE))
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        runtime
            .stage(&dir.join("runtime").join(higher).join(EXE))
            .unwrap();
        assert_eq!(runtime.current().as_deref(), Some(higher));
        runtime.stage(&build(&dir, "b", b"y", 9)).unwrap();
        assert_eq!(runtime.current().as_deref(), Some(higher));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn known_builds_do_not_take_the_install_lock() {
        let dir = tmp("lookup");
        let runtime = Runtime::new(dir.join("runtime"));
        let old = build(&dir, "old", b"old", 1);
        let new = build(&dir, "new", b"new", 2);
        let old_image = runtime.stage(&old).unwrap();
        let new_image = runtime.stage(&new).unwrap();
        let lock = runtime.lock().unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            for source in [old, old_image.exe, new, new_image.exe] {
                runtime.stage(&source).unwrap();
            }
            send.send(()).unwrap();
        });
        let result = receive.recv_timeout(Duration::from_secs(5));
        drop(lock);
        worker.join().unwrap();
        result.unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn reclaimed_identities_are_not_reinstalled_from_relocated_or_retouched_sources() {
        let dir = tmp("receipt");
        let runtime = Runtime::new(dir.join("runtime"));
        let source = build(&dir, "old", b"old", 1);
        let old = runtime.stage(&source).unwrap();
        runtime.stage(&build(&dir, "mid", b"mid", 2)).unwrap();
        let current = runtime.stage(&build(&dir, "new", b"new", 3)).unwrap();
        assert!(!old.exe.exists());
        let receipt = runtime.dir.join(&old.id).join("installed");
        let built = fs::metadata(&receipt).unwrap().modified().unwrap();
        let relocated = build(&dir, "relocated", b"old", 4);
        let found = runtime.stage(&relocated).unwrap();
        assert_eq!(found.id, old.id);
        assert_eq!(found.exe, relocated.canonicalize().unwrap());
        assert!(!old.exe.exists());
        assert_eq!(runtime.current(), Some(current.id));
        assert_eq!(fs::metadata(receipt).unwrap().modified().unwrap(), built);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn read_only_sources_can_be_installed_without_changing_the_source_permissions() {
        let dir = tmp("read-only");
        let runtime = Runtime::new(dir.join("runtime"));
        let source = build(&dir, "source", b"build", 1);
        let original = fs::metadata(&source).unwrap().permissions();
        let mut permissions = original.clone();
        permissions.set_readonly(true);
        fs::set_permissions(&source, permissions.clone()).unwrap();
        let result = runtime.stage(&source);
        assert_eq!(fs::metadata(&source).unwrap().permissions(), permissions);
        fs::set_permissions(&source, original.clone()).unwrap();
        let installed = result.unwrap();
        assert!(
            fs::metadata(&installed.exe)
                .unwrap()
                .permissions()
                .readonly()
        );
        fs::set_permissions(&installed.exe, original).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn a_missing_current_image_is_reinstalled() {
        let dir = tmp("missing");
        let runtime = Runtime::new(dir.join("runtime"));
        let source = build(&dir, "source", b"build", 1);
        let installed = runtime.stage(&source).unwrap();
        fs::remove_file(&installed.exe).unwrap();
        assert_eq!(runtime.stage(&source).unwrap(), installed);
        assert_eq!(fs::read(&installed.exe).unwrap(), b"build");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn a_missing_current_image_cannot_block_an_available_installed_build() {
        let dir = tmp("missing-newer");
        let runtime = Runtime::new(dir.join("runtime"));
        let source = build(&dir, "old", b"old", 1);
        let old = runtime.stage(&source).unwrap();
        let newer = runtime.stage(&build(&dir, "new", b"new", 2)).unwrap();
        fs::remove_file(newer.exe).unwrap();
        assert_eq!(runtime.stage(&source).unwrap(), old);
        assert_eq!(runtime.current(), Some(old.id));
        assert_eq!(fs::read(runtime.current_exe()).unwrap(), b"old");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn identical_content_reuses_the_installed_image_across_source_paths() {
        let dir = tmp("reuse");
        let runtime = Runtime::new(dir.join("runtime"));
        let first = runtime.stage(&build(&dir, "one", b"build", 1)).unwrap();
        let second = runtime.stage(&build(&dir, "two", b"build", 2)).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            fs::metadata(&first.exe).unwrap().modified().unwrap(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_001)
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn timestamp_preserving_source_edits_cannot_mutate_an_installed_identity() {
        let dir = tmp("immutable");
        let runtime = Runtime::new(dir.join("runtime"));
        let first_bytes = vec![b'a'; 64 * 1024];
        let other_bytes = vec![b'b'; 64 * 1024];
        let source = build(&dir, "source", &first_bytes, 1);
        let first = runtime.stage(&source).unwrap();
        let modified = fs::metadata(&source).unwrap().modified().unwrap();
        fs::write(&source, &other_bytes).unwrap();
        File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let second = runtime.stage(&source).unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(fs::read(&first.exe).unwrap(), first_bytes);
        assert_eq!(fs::read(&second.exe).unwrap(), other_bytes);
        fs::remove_dir_all(dir).unwrap();
    }

    fn hash(bytes: &[u8]) -> u64 {
        content_hash(bytes).unwrap()
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn receipts_stamps_and_crash_leftovers_stay_bounded() {
        let dir = tmp("bounded");
        let root = dir.join("runtime");
        let runtime = Runtime::new(root.clone());
        let first = runtime.stage(&build(&dir, "b0", b"build 0", 0)).unwrap();
        fs::write(root.join(&first.id).join(".a.4242"), b"partial").unwrap();
        fs::write(root.join(&first.id).join(".installed.4242"), b"").unwrap();
        fs::write(root.join(".link.4242"), b"").unwrap();
        for n in 1..20 {
            let bytes = format!("build {n}");
            runtime
                .stage(&build(&dir, &format!("b{n}"), bytes.as_bytes(), n))
                .unwrap();
        }
        let ids: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.unwrap().file_name().into_string().ok())
            .filter(|n| n.contains('+'))
            .collect();
        // Two images and eight older receipts, whatever has been installed.
        assert_eq!(ids.len(), 10, "{ids:?}");
        assert_eq!(builds(&root).len(), 2);
        assert!(!root.join(".link.4242").exists());
        assert!(!root.join(&first.id).exists());
        let stamps = fs::read_dir(root.join("stamps")).unwrap().count();
        assert_eq!(stamps, 10);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn crash_leftovers_in_a_kept_build_are_swept() {
        let dir = tmp("sweep");
        let root = dir.join("runtime");
        let runtime = Runtime::new(root.clone());
        let old = runtime.stage(&build(&dir, "old", b"old build", 1)).unwrap();
        fs::write(root.join(&old.id).join(".a.4242"), b"partial").unwrap();
        runtime.stage(&build(&dir, "new", b"new build", 2)).unwrap();
        assert!(!root.join(&old.id).join(".a.4242").exists());
        assert!(root.join(&old.id).join(EXE).is_file());
    }

    #[test]
    fn content_hash_sees_every_byte() {
        let base = vec![7_u8; 1029];
        let h = hash(&base);
        for i in [0, 7, 8, 1023, 1028] {
            let mut changed = base.clone();
            changed[i] ^= 0x80;
            assert_ne!(hash(&changed), h, "byte {i}");
        }
        assert_ne!(hash(&base[..1028]), h);
    }

    /// Reading in blocks names a build as hashing it whole did.
    #[test]
    fn content_hash_is_the_same_across_blocks() {
        let whole = |bytes: &[u8]| {
            let (words, tail) = bytes.as_chunks::<8>();
            let h = words.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, w| {
                (h ^ u64::from_le_bytes(*w))
                    .wrapping_mul(0x0100_0000_01b3)
                    .rotate_left(29)
            });
            fnv1a(tail).wrapping_add(h) ^ bytes.len() as u64
        };
        let bytes: Vec<u8> = (0..200_003_u32).map(|n| (n * 31 % 251) as u8).collect();
        for len in [0, 5, 65_536, 65_541, 131_072, 200_003] {
            assert_eq!(hash(&bytes[..len]), whole(&bytes[..len]), "{len} bytes");
        }
    }
}
