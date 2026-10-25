//! What a directory is to something that would walk all of it and keep
//! up with its changes: a tree on a local disk, one on a filesystem whose
//! change notifications miss what other machines do, or one never walked
//! whole (a volume, the home directory, the system's own trees).
//!
//! And which files are cloud placeholders, whose content (or listing) is
//! fetched from elsewhere when it is read: listed, never opened.

use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};

/// What a directory is to a walk of all of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tree {
    /// On a local filesystem: its native change notifications are complete.
    Watched,
    /// On a filesystem whose notifications miss changes made elsewhere (by
    /// another machine, the host of a VM, a FUSE daemon): it is walked
    /// again to see them.
    Unwatched(Filesystem),
    /// Never walked whole.
    Refused(Refusal),
}

/// Why a tree's change notifications cannot be trusted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filesystem {
    /// NFS, SMB, AFP, `WebDAV` and the like: shared with other machines.
    Network,
    /// Served by a process or a hypervisor (FUSE, 9p, virtiofs, a guest's
    /// shared folders): what it serves changes behind the kernel.
    Virtual,
}

/// Why a directory is never walked whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The root of a volume or a mount: a whole disk.
    Volume,
    /// The user's home directory.
    Home,
    /// The directory every user's home is in.
    Homes,
    /// The system's own trees, or a kernel filesystem (`/proc`, `/sys`).
    System,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Volume => "the root of a volume",
            Self::Home => "the home directory",
            Self::Homes => "the directory of every user's home",
            Self::System => "a system directory",
        })
    }
}

impl Tree {
    /// What `dir` is, links resolved. It asks the filesystem (`statfs`,
    /// the volume's drive type): a network one may take a while to answer,
    /// so this belongs off any latency-sensitive path.
    ///
    /// The root of a whole filesystem is a volume; a mount of a directory
    /// within one (a bind mount, a container's `-v $PWD:/app`) is not. A
    /// volume that is the top of a git working tree is a project and is
    /// walked; `/` and a drive's root never are.
    pub fn of(dir: &Path) -> Self {
        let dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_owned());
        let named = native::named(&dir);
        if let Some(refusal) = Places::native().refusal(&named) {
            return Self::Refused(refusal);
        }
        if named.parent().is_none()
            || (native::is_volume_root(&dir) && !crate::workspace::is_repository_root(&dir))
        {
            return Self::Refused(Refusal::Volume);
        }
        native::filesystem(&dir)
    }
}

/// Whether the platform has cloud placeholders at all: where it has none,
/// a walk need not look at a directory's metadata to find them.
pub const PLACEHOLDERS: bool = cfg!(any(target_os = "macos", windows));

/// Whether `meta` (as a directory listing gives it, links not followed)
/// is a cloud placeholder: a file whose content, or a directory whose
/// listing, is fetched from a remote store when read. An iCloud Drive or
/// File Provider dataless file on macOS (`SF_DATALESS`); on Windows, a
/// Cloud Files placeholder (`FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`,
/// `RECALL_ON_OPEN`) or an offline file (`FILE_ATTRIBUTE_OFFLINE`).
pub fn is_placeholder(meta: &Metadata) -> bool {
    native::placeholder(meta)
}

/// [`is_placeholder`] for what is at `path`, asked without opening it: on
/// Windows, opening a placeholder can be what fetches it.
pub(crate) fn placeholder_at(path: &Path) -> io::Result<bool> {
    native::placeholder_at(path)
}

/// While held, a placeholder this thread reads fails to read instead of
/// being fetched: on macOS, Apple's own opt-out for indexers
/// (`setiopolicy_np(IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
/// IOPOL_SCOPE_THREAD, OFF)`), which covers a file evicted after
/// [`is_placeholder`] said it was not one. Dropping it restores the
/// thread's policy. Elsewhere the attributes checked before opening are
/// the guard, and this does nothing.
pub struct NeverFetch {
    #[cfg(target_os = "macos")]
    previous: Option<std::ffi::c_int>,
    /// The policy is the thread's: the guard stays on it.
    _thread: std::marker::PhantomData<*const ()>,
}

impl NeverFetch {
    pub fn new() -> Self {
        Self {
            #[cfg(target_os = "macos")]
            previous: native::never_fetch(),
            _thread: std::marker::PhantomData,
        }
    }
}

impl Default for NeverFetch {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for NeverFetch {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(previous) = self.previous {
            native::restore_fetch(previous);
        }
    }
}

/// The directories never walked whole, as this platform names them.
struct Places {
    home: Option<PathBuf>,
    homes: Vec<PathBuf>,
    /// Refused themselves; what is under them is not.
    system: Vec<PathBuf>,
    /// Refused with everything under them.
    sealed: Vec<PathBuf>,
    /// Another system's directories of homes, as seen from this one (WSL's
    /// `/mnt/c/Users`): refused, and each home in them.
    foreign_homes: Vec<PathBuf>,
}

impl Places {
    fn native() -> Self {
        let canonical = |path: PathBuf| fs::canonicalize(&path).unwrap_or(path);
        let places = native::places();
        let home = places.home.map(canonical);
        let mut homes: Vec<PathBuf> = places.homes.into_iter().map(canonical).collect();
        homes.extend(home.as_deref().and_then(Path::parent).map(Path::to_owned));
        Self {
            home,
            homes,
            system: places.system.into_iter().map(canonical).collect(),
            sealed: places.sealed.into_iter().map(canonical).collect(),
            foreign_homes: places.foreign_homes,
        }
    }

    fn refusal(&self, dir: &Path) -> Option<Refusal> {
        if self.home.as_deref() == Some(dir) {
            Some(Refusal::Home)
        } else if self
            .homes
            .iter()
            .chain(&self.foreign_homes)
            .any(|homes| homes == dir)
        {
            Some(Refusal::Homes)
        } else if (dir.parent())
            .is_some_and(|parent| self.foreign_homes.iter().any(|f| f == parent))
        {
            Some(Refusal::Home)
        } else if self.system.iter().any(|system| system == dir)
            || self.sealed.iter().any(|sealed| dir.starts_with(sealed))
        {
            Some(Refusal::System)
        } else {
            None
        }
    }
}

#[cfg(unix)]
mod unix {
    use std::path::PathBuf;

    use super::Places;

    /// The Filesystem Hierarchy Standard's top-level directories, and the
    /// user's home as `HOME` or the password database names it.
    pub(super) fn places(
        homes: &str,
        more: &[&str],
        sealed: &[&str],
        foreign_homes: Vec<PathBuf>,
    ) -> Places {
        const FHS: [&str; 19] = [
            "/bin", "/boot", "/dev", "/etc", "/lib", "/lib32", "/lib64", "/libx32", "/media",
            "/mnt", "/opt", "/proc", "/run", "/sbin", "/srv", "/sys", "/tmp", "/usr", "/var",
        ];
        Places {
            home: std::env::home_dir(),
            homes: vec![PathBuf::from(homes)],
            system: (FHS.iter().chain(&["/nix", "/nix/store"]).chain(more))
                .map(PathBuf::from)
                .collect(),
            sealed: sealed.iter().map(PathBuf::from).collect(),
            foreign_homes,
        }
    }
}

#[cfg(target_os = "macos")]
mod native {
    use std::ffi::c_int;
    use std::fs::Metadata;
    use std::io;
    use std::os::macos::fs::MetadataExt;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use super::{Filesystem, Places, Refusal, Tree};

    /// `<sys/stat.h>`: the file's data (or a directory's listing) is not
    /// on the device; reading it materializes it from its provider.
    const SF_DATALESS: u32 = 0x4000_0000;

    pub(super) fn places() -> Places {
        // `/System` is the sealed system volume and, at
        // `/System/Volumes/Data`, every other volume's data again.
        super::unix::places(
            "/Users",
            &[
                "/Applications",
                "/Library",
                "/Volumes",
                "/cores",
                "/private",
            ],
            &["/System"],
            Vec::new(),
        )
    }

    /// The Data volume's firmlinked trees under the names they have:
    /// `/System/Volumes/Data/Users/me` is `/Users/me`, and the volume's
    /// own root is `/`'s other half.
    pub(super) fn named(dir: &Path) -> PathBuf {
        match dir.strip_prefix("/System/Volumes/Data") {
            Ok(rest) => Path::new("/").join(rest),
            Err(_) => dir.to_owned(),
        }
    }

    /// A volume is mounted at `dir`: `statfs` names it as the mount's
    /// point. A firmlink (`/Users`) is on the Data volume, mounted
    /// elsewhere.
    pub(super) fn is_volume_root(dir: &Path) -> bool {
        rustix::fs::statfs(dir).is_ok_and(|stat| {
            let point: Vec<u8> = (stat.f_mntonname.iter())
                .take_while(|&&c| c != 0)
                .map(|&c| c.to_ne_bytes()[0])
                .collect();
            Path::new(std::ffi::OsStr::from_bytes(&point)) == dir
        })
    }

    /// What `statfs` says: `MNT_LOCAL` unset for a network volume, the
    /// filesystem's name for FUSE and the kernel's own.
    pub(super) fn filesystem(dir: &Path) -> Tree {
        let Ok(stat) = rustix::fs::statfs(dir) else {
            return Tree::Watched;
        };
        let name: Vec<u8> = (stat.f_fstypename.iter())
            .take_while(|&&c| c != 0)
            .map(|&c| c.to_ne_bytes()[0])
            .collect();
        let local = stat.f_flags & libc::MNT_LOCAL.cast_unsigned() != 0;
        kind(&name, local)
    }

    pub(super) fn kind(name: &[u8], local: bool) -> Tree {
        match name {
            b"devfs" => Tree::Refused(Refusal::System),
            b"nfs" | b"smbfs" | b"afpfs" | b"webdav" | b"cifs" | b"ftp" => {
                Tree::Unwatched(Filesystem::Network)
            }
            _ if name.starts_with(b"macfuse")
                || name.starts_with(b"osxfuse")
                || name.starts_with(b"fuse") =>
            {
                Tree::Unwatched(Filesystem::Virtual)
            }
            _ if !local => Tree::Unwatched(Filesystem::Network),
            _ => Tree::Watched,
        }
    }

    pub(super) fn placeholder(meta: &Metadata) -> bool {
        meta.st_flags() & SF_DATALESS != 0
    }

    // `<sys/resource.h>`.
    const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: c_int = 3;
    const IOPOL_SCOPE_THREAD: c_int = 1;
    const IOPOL_MATERIALIZE_DATALESS_FILES_OFF: c_int = 1;

    // SAFETY: both take plain integers and touch only the I/O policy of
    // the calling thread (or process): any arguments are sound.
    unsafe extern "C" {
        safe fn getiopolicy_np(iotype: c_int, scope: c_int) -> c_int;
        safe fn setiopolicy_np(iotype: c_int, scope: c_int, policy: c_int) -> c_int;
    }

    /// Turns materializing off for this thread: the policy it had, or
    /// `None` if it could not be changed.
    pub(super) fn never_fetch() -> Option<c_int> {
        let kind = IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES;
        let previous = getiopolicy_np(kind, IOPOL_SCOPE_THREAD);
        (previous >= 0
            && setiopolicy_np(
                kind,
                IOPOL_SCOPE_THREAD,
                IOPOL_MATERIALIZE_DATALESS_FILES_OFF,
            ) == 0)
            .then_some(previous)
    }

    pub(super) fn restore_fetch(previous: c_int) {
        setiopolicy_np(
            IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
            IOPOL_SCOPE_THREAD,
            previous,
        );
    }

    pub(super) fn placeholder_at(path: &Path) -> io::Result<bool> {
        // `lstat` reads the inode, never the data: it materializes nothing.
        Ok(placeholder(&std::fs::symlink_metadata(path)?))
    }
}

#[cfg(target_os = "linux")]
mod native {
    use std::fs::Metadata;
    use std::io;
    use std::path::{Path, PathBuf};

    use std::os::unix::fs::MetadataExt;

    use rustix::fs::{AtFlags, CWD, StatxAttributes, StatxFlags};

    use super::{Filesystem, Places, Refusal, Tree};

    pub(super) fn places() -> Places {
        super::unix::places("/home", &["/root", "/snap"], &[], windows_homes())
    }

    /// The `Users` directory of each Windows drive WSL mounts (`/mnt/c`):
    /// the Windows homes, seen from Linux.
    fn windows_homes() -> Vec<PathBuf> {
        let Ok(mounts) = std::fs::read_dir("/mnt") else {
            return Vec::new();
        };
        mounts
            .flatten()
            .filter(|drive| {
                let name = drive.file_name();
                name.len() == 1 && name.as_encoded_bytes()[0].is_ascii_alphabetic()
            })
            .map(|drive| drive.path().join("Users"))
            .collect()
    }

    pub(super) fn named(dir: &Path) -> PathBuf {
        dir.to_owned()
    }

    /// A whole filesystem is mounted at `dir`: `statx` says a mount is
    /// rooted there, and `/proc/self/mountinfo` says it mounts its
    /// filesystem's root, not a directory within it as a bind mount (a
    /// container's `-v $PWD:/app`) or a btrfs subvolume does. Before Linux
    /// 5.8, a change of device is all there is to go by.
    pub(super) fn is_volume_root(dir: &Path) -> bool {
        let asked = StatxFlags::MNT_ID;
        let Ok(stat) = rustix::fs::statx(CWD, dir, AtFlags::NO_AUTOMOUNT, asked) else {
            return is_mount_root(dir);
        };
        if !stat
            .stx_attributes_mask
            .contains(StatxAttributes::MOUNT_ROOT)
            || !StatxFlags::from_bits_retain(stat.stx_mask).contains(asked)
        {
            return is_mount_root(dir);
        }
        stat.stx_attributes.contains(StatxAttributes::MOUNT_ROOT)
            && std::fs::read_to_string("/proc/self/mountinfo")
                .map_or(true, |mounts| mounts_whole(&mounts, stat.stx_mnt_id))
    }

    /// Another filesystem is mounted at `dir`: it is on another device
    /// than its parent. Where nothing better says so: a bind mount and a
    /// btrfs subvolume look the same.
    fn is_mount_root(dir: &Path) -> bool {
        let device = |path: &Path| std::fs::metadata(path).map(|meta| meta.dev()).ok();
        let here = device(dir);
        dir.parent()
            .is_some_and(|parent| here.is_some() && here != device(parent))
    }

    /// Whether mount `id` of `mountinfo` (proc(5)) mounts its filesystem's
    /// root: its fourth field is `/`.
    pub(super) fn mounts_whole(mountinfo: &str, id: u64) -> bool {
        let id = id.to_string();
        mountinfo
            .lines()
            .map(|line| line.split(' '))
            .find_map(|mut fields| (fields.next() == Some(&id)).then(|| fields.nth(2)))
            .flatten()
            .is_none_or(|root| root == "/")
    }

    /// What `statfs`'s `f_type` says, by the kernel's magic numbers
    /// (`linux/magic.h`).
    pub(super) fn filesystem(dir: &Path) -> Tree {
        let Ok(stat) = rustix::fs::statfs(dir) else {
            return Tree::Watched;
        };
        // The magic numbers are 32 bits; `f_type` is a signed word on most
        // ABIs, so one above `i32::MAX` comes back sign-extended.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let magic = stat.f_type as u32;
        kind(magic)
    }

    pub(super) fn kind(magic: u32) -> Tree {
        match magic {
            // NFS, SMB, CIFS, SMB2, Ceph, OpenAFS, kAFS, Coda, GFS2, Lustre.
            0x6969 | 0x517B | 0xFF53_4D42 | 0xFE53_4D42 | 0x00C3_6400 | 0x5346_414F
            | 0x6B41_4653 | 0x7375_7245 | 0x0116_1970 | 0x0BD0_0BD0 => {
                Tree::Unwatched(Filesystem::Network)
            }
            // FUSE (virtiofs too), 9p (WSL 2's Windows drives), WSL 1's
            // drvfs, VirtualBox's shared folders.
            0x6573_5546 | 0x0102_1997 | 0x5346_4846 | 0x786F_4256 => {
                Tree::Unwatched(Filesystem::Virtual)
            }
            // proc, sysfs, cgroup, cgroup2, debugfs, tracefs, securityfs,
            // pstore, bpf, configfs, devpts, efivarfs.
            0x9FA0 | 0x6265_6572 | 0x0027_E0EB | 0x6367_7270 | 0x6462_6720 | 0x7472_6163
            | 0x7363_6673 | 0x6165_676C | 0xCAFE_4A11 | 0x6265_6570 | 0x1CD1 | 0xDE5E_81E4 => {
                Tree::Refused(Refusal::System)
            }
            // overlayfs reports changes made through it, as a container's
            // root makes them.
            _ => Tree::Watched,
        }
    }

    pub(super) fn placeholder(_: &Metadata) -> bool {
        false
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "the other platforms' lookup can fail; this one asks nothing"
    )]
    pub(super) fn placeholder_at(_: &Path) -> io::Result<bool> {
        Ok(false)
    }
}

#[cfg(windows)]
mod native {
    use std::fs::Metadata;
    use std::io;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::fs::MetadataExt;
    use std::path::{Component, Path, PathBuf, Prefix};

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
        FILE_ATTRIBUTE_RECALL_ON_OPEN, GetDriveTypeW, GetFileAttributesW, GetVolumeInformationW,
        GetVolumePathNameW, INVALID_FILE_ATTRIBUTES,
    };
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_Profile, FOLDERID_ProgramData, FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86,
        FOLDERID_UserProfiles, FOLDERID_Windows,
    };

    use super::{Filesystem, Places, Tree};
    use crate::windows::wide;

    /// `GetDriveTypeW`: a network drive (`WinBase.h`).
    const DRIVE_REMOTE: u32 = 4;
    /// Room for a filesystem's name: `MAX_PATH + 1`, as the documentation
    /// of `GetVolumeInformationW` sizes it.
    const FILESYSTEM_NAME: u32 = 261;
    const PLACEHOLDER: u32 = FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
        | FILE_ATTRIBUTE_RECALL_ON_OPEN
        | FILE_ATTRIBUTE_OFFLINE;

    /// The shell's known folders: the profile, the profiles' directory,
    /// Program Files and `ProgramData`, and the Windows directory with all
    /// under it.
    pub(super) fn places() -> Places {
        let folder = |id| crate::paths::known_folder(id).ok();
        Places {
            home: folder(&FOLDERID_Profile),
            homes: folder(&FOLDERID_UserProfiles).into_iter().collect(),
            system: [
                &FOLDERID_ProgramFiles,
                &FOLDERID_ProgramFilesX86,
                &FOLDERID_ProgramData,
            ]
            .into_iter()
            .filter_map(folder)
            .collect(),
            sealed: folder(&FOLDERID_Windows).into_iter().collect(),
            foreign_homes: Vec::new(),
        }
    }

    pub(super) fn named(dir: &Path) -> PathBuf {
        dir.to_owned()
    }

    /// A volume is mounted at `dir`: it is its own volume path, as a drive
    /// letter's root or a mounted folder is.
    pub(super) fn is_volume_root(dir: &Path) -> bool {
        volume_path(dir).is_some_and(|volume| volume == dunce::simplified(dir))
    }

    /// A UNC path (`\\server\share`, `\\wsl$\…`) or a network drive is on
    /// the network; a volume whose filesystem is none Windows ships is
    /// served by a driver (`WinFsp`, Dokan) and is virtual.
    pub(super) fn filesystem(dir: &Path) -> Tree {
        let unc = matches!(
            dir.components().next(),
            Some(Component::Prefix(prefix))
                if matches!(prefix.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..))
        );
        if unc {
            return Tree::Unwatched(Filesystem::Network);
        }
        let Some(volume) = volume_path(dir) else {
            return Tree::Watched;
        };
        let mut root = wide(volume.as_os_str());
        if root.len() > 1 && root[root.len() - 2] != u16::from(b'\\') {
            root.insert(root.len() - 1, u16::from(b'\\'));
        }
        // SAFETY: root is NUL-terminated.
        if unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_REMOTE {
            return Tree::Unwatched(Filesystem::Network);
        }
        let mut name = [0u16; FILESYSTEM_NAME as usize];
        // SAFETY: root is NUL-terminated, name is writable for its length,
        // and the outputs not asked for are null.
        let named = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                name.as_mut_ptr(),
                FILESYSTEM_NAME,
            )
        };
        if named == 0 {
            return Tree::Watched;
        }
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        kind(&String::from_utf16_lossy(&name[..len]))
    }

    pub(super) fn kind(filesystem: &str) -> Tree {
        match filesystem {
            "NTFS" | "ReFS" | "FAT" | "FAT32" | "exFAT" | "CDFS" | "UDF" => Tree::Watched,
            _ => Tree::Unwatched(Filesystem::Virtual),
        }
    }

    /// The root of the volume `dir` is on (`C:\`, a mounted folder), in
    /// the spelling `dir` has.
    fn volume_path(dir: &Path) -> Option<PathBuf> {
        let dir = wide(dunce::simplified(dir).as_os_str());
        let mut volume = vec![0u16; dir.len() + 1];
        let capacity = u32::try_from(volume.len()).ok()?;
        // SAFETY: dir is NUL-terminated and volume is writable for its
        // length, which a volume path of dir never exceeds.
        let found = unsafe { GetVolumePathNameW(dir.as_ptr(), volume.as_mut_ptr(), capacity) };
        if found == 0 {
            return None;
        }
        let len = volume.iter().position(|&c| c == 0).unwrap_or(volume.len());
        Some(PathBuf::from(std::ffi::OsString::from_wide(&volume[..len])))
    }

    pub(super) fn placeholder(meta: &Metadata) -> bool {
        meta.file_attributes() & PLACEHOLDER != 0
    }

    pub(super) fn placeholder_at(path: &Path) -> io::Result<bool> {
        let path = wide(verbatim(path)?.as_os_str());
        // SAFETY: path is NUL-terminated. Attributes come from the
        // directory entry: no handle is opened, nothing is recalled.
        let attributes = unsafe { GetFileAttributesW(path.as_ptr()) };
        if attributes == INVALID_FILE_ATTRIBUTES {
            return Err(io::Error::last_os_error());
        }
        Ok(attributes & PLACEHOLDER != 0)
    }

    /// `path` made absolute (`GetFullPathNameW`, which opens nothing) and
    /// verbatim, so a path past `MAX_PATH` is asked about as it is.
    fn verbatim(path: &Path) -> io::Result<PathBuf> {
        let absolute = std::path::absolute(path)?;
        let Some(Component::Prefix(prefix)) = absolute.components().next() else {
            return Ok(absolute);
        };
        // A UNC path drops one of its two leading backslashes.
        let (head, skip) = match prefix.kind() {
            Prefix::Disk(_) => (r"\\?\", 0),
            Prefix::UNC(..) => (r"\\?\UNC", 1),
            _ => return Ok(absolute),
        };
        let mut verbatim = std::ffi::OsString::from(head);
        let rest = absolute.as_os_str().encode_wide().collect::<Vec<_>>();
        verbatim.push(std::ffi::OsString::from_wide(&rest[skip..]));
        Ok(PathBuf::from(verbatim))
    }

    /// Marks `path` offline, as Remote Storage would: the one placeholder
    /// attribute a program may set.
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn mark_placeholder(path: &Path) -> io::Result<bool> {
        use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;
        let attributes = std::fs::metadata(path)?.file_attributes();
        let path = wide(verbatim(path)?.as_os_str());
        // SAFETY: path is NUL-terminated.
        if unsafe { SetFileAttributesW(path.as_ptr(), attributes | FILE_ATTRIBUTE_OFFLINE) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(true)
    }
}

/// Makes `path` a placeholder where a program may (an offline file on
/// Windows), for tests of what never opens one. `false` where the
/// platform lets no program do it: macOS sets `SF_DATALESS` only for a
/// File Provider, and Linux has no placeholders.
#[cfg(any(test, feature = "test-support"))]
pub fn mark_placeholder(path: &Path) -> io::Result<bool> {
    #[cfg(windows)]
    return native::mark_placeholder(path);
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_home_directory_is_never_walked_whole() {
        let home = std::env::home_dir().unwrap();
        assert_eq!(Tree::of(&home), Tree::Refused(Refusal::Home));
        assert_eq!(
            Tree::of(home.parent().unwrap()),
            Tree::Refused(Refusal::Homes)
        );
    }

    #[test]
    fn a_volume_root_is_never_walked_whole() {
        let temp = fs::canonicalize(std::env::temp_dir()).unwrap();
        let volume = temp.ancestors().last().unwrap();
        assert_eq!(Tree::of(volume), Tree::Refused(Refusal::Volume));
    }

    #[test]
    fn a_project_directory_on_a_local_disk_is_watched() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        fs::create_dir(&project).unwrap();
        assert_eq!(Tree::of(&project), Tree::Watched);
    }

    #[test]
    fn a_sealed_system_tree_is_refused_with_everything_under_it() {
        let places = Places {
            home: Some(PathBuf::from("/h/me")),
            homes: vec![PathBuf::from("/h")],
            system: vec![PathBuf::from("/usr")],
            sealed: vec![PathBuf::from("/System")],
            foreign_homes: vec![PathBuf::from("/mnt/c/Users")],
        };
        let refusal = |dir: &str| places.refusal(Path::new(dir));
        assert_eq!(refusal("/h/me"), Some(Refusal::Home));
        assert_eq!(refusal("/h"), Some(Refusal::Homes));
        assert_eq!(refusal("/h/me/project"), None);
        assert_eq!(refusal("/usr"), Some(Refusal::System));
        assert_eq!(refusal("/usr/src/linux"), None);
        assert_eq!(refusal("/System/Library"), Some(Refusal::System));
        assert_eq!(refusal("/mnt/c/Users"), Some(Refusal::Homes));
        assert_eq!(refusal("/mnt/c/Users/me"), Some(Refusal::Home));
        assert_eq!(refusal("/mnt/c/Users/me/project"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_home_through_the_data_volume_is_the_home_on_macos() {
        let home = fs::canonicalize(std::env::home_dir().unwrap()).unwrap();
        let data = Path::new("/System/Volumes/Data").join(home.strip_prefix("/").unwrap());
        assert_eq!(Tree::of(&data), Tree::Refused(Refusal::Home));
        let project = tempfile::tempdir_in(&home).unwrap();
        let through_data =
            Path::new("/System/Volumes/Data").join(project.path().strip_prefix("/").unwrap());
        assert_eq!(Tree::of(&through_data), Tree::Watched);
        assert_eq!(
            Tree::of(Path::new("/System/Volumes/Data")),
            Tree::Refused(Refusal::Volume)
        );
        // A firmlink is not a volume of its own.
        assert!(!native::is_volume_root(Path::new("/Users")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_bind_mount_is_a_directory_and_a_whole_filesystem_a_volume() {
        let mountinfo = "\
22 1 8:1 / / rw - ext4 /dev/sda1 rw
40 22 8:1 /home/me/project /app rw - ext4 /dev/sda1 rw
41 22 8:17 / /data rw - xfs /dev/sdb1 rw
42 22 0:30 /@home /home rw - btrfs /dev/sdc rw
";
        assert!(native::mounts_whole(mountinfo, 22));
        assert!(!native::mounts_whole(mountinfo, 40));
        assert!(native::mounts_whole(mountinfo, 41));
        assert!(!native::mounts_whole(mountinfo, 42));
        assert!(native::is_volume_root(Path::new("/proc")));
        assert!(!native::is_volume_root(Path::new("/proc/self")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn network_and_fuse_volumes_are_unwatched_on_macos() {
        use native::kind;
        assert_eq!(kind(b"apfs", true), Tree::Watched);
        assert_eq!(kind(b"smbfs", false), Tree::Unwatched(Filesystem::Network));
        assert_eq!(kind(b"nfs", true), Tree::Unwatched(Filesystem::Network));
        assert_eq!(kind(b"macfuse", true), Tree::Unwatched(Filesystem::Virtual));
        assert_eq!(kind(b"exotic", false), Tree::Unwatched(Filesystem::Network));
        assert_eq!(kind(b"devfs", true), Tree::Refused(Refusal::System));
        assert_eq!(
            Tree::of(Path::new("/dev/fd")),
            Tree::Refused(Refusal::System)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn network_virtual_and_kernel_filesystems_are_told_apart_on_linux() {
        use native::kind;
        assert_eq!(kind(0xEF53), Tree::Watched); // ext4
        assert_eq!(kind(0x794C_7630), Tree::Watched); // overlayfs
        assert_eq!(kind(0x6969), Tree::Unwatched(Filesystem::Network));
        assert_eq!(kind(0xFE53_4D42), Tree::Unwatched(Filesystem::Network));
        assert_eq!(kind(0x6573_5546), Tree::Unwatched(Filesystem::Virtual));
        assert_eq!(kind(0x0102_1997), Tree::Unwatched(Filesystem::Virtual));
        assert_eq!(kind(0x5346_4846), Tree::Unwatched(Filesystem::Virtual));
        assert_eq!(kind(0x9FA0), Tree::Refused(Refusal::System));
        assert_eq!(
            Tree::of(Path::new("/proc/self")),
            Tree::Refused(Refusal::System)
        );
    }

    #[cfg(windows)]
    #[test]
    fn only_the_filesystems_windows_ships_are_watched() {
        use native::kind;
        assert_eq!(kind("NTFS"), Tree::Watched);
        assert_eq!(kind("ReFS"), Tree::Watched);
        assert_eq!(kind("FUSE-WinFsp"), Tree::Unwatched(Filesystem::Virtual));
        assert_eq!(
            Tree::of(Path::new(r"\\localhost\C$\Windows\Temp")),
            Tree::Unwatched(Filesystem::Network)
        );
    }

    #[test]
    fn never_fetching_leaves_resident_files_readable_and_the_thread_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("local.txt");
        fs::write(&file, "text").unwrap();
        #[cfg(target_os = "macos")]
        let before = native::never_fetch().inspect(|&p| native::restore_fetch(p));
        {
            let _never = NeverFetch::new();
            assert_eq!(fs::read_to_string(&file).unwrap(), "text");
        }
        #[cfg(target_os = "macos")]
        assert_eq!(
            native::never_fetch().inspect(|&p| native::restore_fetch(p)),
            before
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_placeholder_is_found_past_max_path() {
        let dir = tempfile::tempdir().unwrap();
        let deep = (0..30).fold(dir.path().to_owned(), |path, i| {
            path.join(format!("deep{i:02}"))
        });
        fs::create_dir_all(&deep).unwrap();
        let file = deep.join("cloud.txt");
        fs::write(&file, "text").unwrap();
        assert!(file.as_os_str().len() > 260);
        assert!(!placeholder_at(&file).unwrap());
        assert!(mark_placeholder(&file).unwrap());
        assert!(placeholder_at(&file).unwrap());
    }

    #[test]
    fn a_placeholder_is_found_without_opening_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cloud.txt");
        fs::write(&file, "text").unwrap();
        assert!(!placeholder_at(&file).unwrap());
        assert!(!is_placeholder(&fs::symlink_metadata(&file).unwrap()));
        if mark_placeholder(&file).unwrap() {
            assert!(placeholder_at(&file).unwrap());
            assert!(is_placeholder(&fs::symlink_metadata(&file).unwrap()));
        }
    }
}
