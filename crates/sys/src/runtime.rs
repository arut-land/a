//! Staged builds of `a` under `<home>/runtime/<id>/a`. Whichever entry point
//! installed `a` (app, brew, cargo, mise), it stages itself here first and the
//! daemon only ever runs `runtime/current`, so the newest build wins.

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::fnv1a;

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
        self.dir.join("current").join("a")
    }

    /// The id `current` points at.
    pub fn current(&self) -> Option<String> {
        let target = fs::read_link(self.dir.join("current")).ok()?;
        target.into_os_string().into_string().ok()
    }

    /// Stages `exe` (copy, verify, then repoint `current` if it is newer)
    /// and keeps the two newest builds. Hashes the binary only the first time
    /// a given file is seen: a stamp keyed by its path and metadata remembers
    /// the id. A build already outranked (staged once, then pruned) is not
    /// copied again, so an older `a` that keeps being run
    /// costs a lookup, not a copy.
    pub fn stage(&self, exe: &Path) -> io::Result<Staged> {
        let exe = fs::canonicalize(exe)?;
        let known = self.id_in_runtime(&exe).or_else(|| self.stamped(&exe));
        let id = match known {
            Some(id) if self.current().as_ref() == Some(&id) || self.outranked(&id) => id,
            known => {
                let _lock = self.lock()?;
                let id = match known.filter(|id| self.is_staged(id)) {
                    Some(id) => id,
                    None => self.copy_in(&exe)?,
                };
                self.promote(&id)?;
                id
            }
        };
        let exe = Some(self.dir.join(&id).join("a"))
            .filter(|staged| staged.is_file())
            .unwrap_or(exe);
        Ok(Staged { id, exe })
    }

    /// The id of an exe that already is `runtime/<id>/a`.
    fn id_in_runtime(&self, exe: &Path) -> Option<String> {
        let dir = exe.parent()?;
        if exe.file_name()? != "a" || dir.parent()? != fs::canonicalize(&self.dir).ok()? {
            return None;
        }
        dir.file_name()?.to_str().map(str::to_owned)
    }

    fn stamp(&self, exe: &Path) -> io::Result<PathBuf> {
        let meta = fs::metadata(exe)?;
        let mut key = exe.as_os_str().as_encoded_bytes().to_vec();
        for n in [meta.dev(), meta.ino(), meta.size()] {
            key.extend(n.to_le_bytes());
        }
        key.extend(meta.mtime().to_le_bytes());
        key.extend(meta.mtime_nsec().to_le_bytes());
        Ok(self
            .dir
            .join("stamps")
            .join(format!("{:016x}", fnv1a(&key))))
    }

    /// The id a stamp remembers for `exe`, staged or since pruned.
    fn stamped(&self, exe: &Path) -> Option<String> {
        let id = fs::read_link(self.stamp(exe).ok()?).ok()?.into_os_string();
        id.into_string().ok()
    }

    fn is_staged(&self, id: &str) -> bool {
        self.dir.join(id).join("a").is_file()
    }

    /// A known build that is no longer staged while `current` is: only
    /// builds older than the two kept are pruned, and `current` never moves
    /// back, so it can never win again.
    fn outranked(&self, id: &str) -> bool {
        !self.is_staged(id)
            && self
                .current()
                .is_some_and(|c| c != id && self.is_staged(&c))
    }

    /// Copies `exe` to `runtime/<id>/a` through a temporary file, checks the
    /// copy's hash, and renames it into place.
    fn copy_in(&self, exe: &Path) -> io::Result<String> {
        let hash = content_hash(File::open(exe)?)?;
        let id = format!("{VERSION}+{hash:016x}");
        let dir = self.dir.join(&id);
        let target = dir.join("a");
        if !target.is_file() {
            fs::create_dir_all(&dir)?;
            let tmp = dir.join(format!(".a.{}", std::process::id()));
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o755)
                .open(&tmp)?;
            io::copy(&mut File::open(exe)?, &mut file)?;
            // The build's own time orders builds of one version.
            file.set_modified(fs::metadata(exe)?.modified()?)?;
            file.sync_all()?;
            if content_hash(File::open(&tmp)?)? != hash {
                let _ = fs::remove_file(&tmp);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the staged copy does not match its source",
                ));
            }
            fs::rename(&tmp, &target)?;
        }
        let stamp = self.stamp(exe)?;
        fs::create_dir_all(stamp.parent().unwrap_or(&self.dir))?;
        replace_symlink(&id, &stamp)?;
        Ok(id)
    }

    /// Points `current` at `id` if it is newer than what `current` names,
    /// never older, then prunes.
    fn promote(&self, id: &str) -> io::Result<()> {
        let newer = match self.current().and_then(|current| self.order(&current)) {
            Some(current) => self.order(id).is_some_and(|order| order > current),
            None => true,
        };
        if newer {
            replace_symlink(id, &self.dir.join("current"))?;
        }
        self.prune()
    }

    /// Keeps `current` and the newest other build. Stamps of removed builds
    /// are what lets an older `a` skip staging, so the newest few of them
    /// are kept too.
    fn prune(&self) -> io::Result<()> {
        /// Enough for the few older installs one machine may keep running.
        const OUTRANKED: usize = 8;
        let current = self.current();
        let mut others: Vec<(Order, String)> = fs::read_dir(&self.dir)?
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|id| id.contains('+') && Some(id) != current.as_ref())
            .filter_map(|id| Some((self.order(&id)?, id)))
            .collect();
        others.sort_by(|a, b| b.cmp(a));
        for (_, id) in others.into_iter().skip(1) {
            fs::remove_dir_all(self.dir.join(id))?;
        }
        let Ok(stamps) = fs::read_dir(self.dir.join("stamps")) else {
            return Ok(());
        };
        let mut dangling: Vec<(SystemTime, PathBuf)> = stamps
            .flatten()
            .map(|stamp| stamp.path())
            .filter(|path| fs::read_link(path).is_ok_and(|id| !self.dir.join(id).exists()))
            .filter_map(|path| Some((fs::symlink_metadata(&path).ok()?.modified().ok()?, path)))
            .collect();
        dangling.sort_by(|a, b| b.cmp(a));
        for (_, path) in dangling.into_iter().skip(OUTRANKED) {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Builds order by version, then by build time; `None` if not staged.
    fn order(&self, id: &str) -> Option<Order> {
        let built = fs::metadata(self.dir.join(id).join("a"))
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
        fs::create_dir_all(&self.dir)?;
        let file = File::create(self.dir.join(".lock"))?;
        file.lock()?;
        Ok(file)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Order {
    version: Vec<u64>,
    built: SystemTime,
}

/// Atomically replaces `link` with a symlink to `target` (relative).
fn replace_symlink(target: &str, link: &Path) -> io::Result<()> {
    let tmp = link.with_file_name(format!(".link.{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    symlink(target, &tmp)?;
    fs::rename(&tmp, link)
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
            .filter(|n| n.contains('+'))
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
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o555)).unwrap();
        let restaged = runtime.stage(&dir.join("bin/old2"));
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(restaged.unwrap(), again);

        // A known file is recognised by its stamp; the copy is never redone.
        fs::write(&new.exe, b"tampered").unwrap();
        assert_eq!(runtime.stage(&dir.join("bin/new")).unwrap().id, new.id);
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
        fs::copy(&staged.exe, dir.join("runtime").join(higher).join("a")).unwrap();
        File::options()
            .write(true)
            .open(dir.join("runtime").join(higher).join("a"))
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        runtime.promote(higher).unwrap();
        assert_eq!(runtime.current().as_deref(), Some(higher));
        runtime.stage(&build(&dir, "b", b"y", 9)).unwrap();
        assert_eq!(runtime.current().as_deref(), Some(higher));
        fs::remove_dir_all(&dir).unwrap();
    }

    fn hash(bytes: &[u8]) -> u64 {
        content_hash(bytes).unwrap()
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
