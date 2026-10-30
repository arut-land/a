//! Executable installation preserves provenance and never shares writable
//! source inodes with an installed build.

use std::fs::{self, File};
use std::io;
use std::path::Path;

#[cfg(target_os = "linux")]
pub(super) fn copy(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let source = File::open(source)?;
    let mut target = File::options().write(true).create_new(true).open(target)?;
    match rustix::fs::ioctl_ficlone(&target, &source) {
        Ok(()) => {}
        Err(
            rustix::io::Errno::OPNOTSUPP
            | rustix::io::Errno::XDEV
            | rustix::io::Errno::INVAL
            | rustix::io::Errno::NOTTY,
        ) => {
            io::copy(&mut &source, &mut target)?;
        }
        Err(error) => return Err(error.into()),
    }
    let metadata = source.metadata()?;
    target.set_permissions(fs::Permissions::from_mode(
        metadata.permissions().mode() | 0o100,
    ))?;
    target.set_modified(metadata.modified()?)
}

#[cfg(target_os = "macos")]
pub(super) fn copy(source: &Path, target: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, CloneFlags};
    use std::os::unix::fs::PermissionsExt;

    let source = File::open(source)?;
    match rustix::fs::fclonefileat(&source, CWD, target, CloneFlags::empty()) {
        Ok(()) => {}
        Err(
            rustix::io::Errno::OPNOTSUPP
            | rustix::io::Errno::NOTSUP
            | rustix::io::Errno::XDEV
            | rustix::io::Errno::INVAL,
        ) => copy_metadata(&source, target)?,
        Err(error) => return Err(error.into()),
    }
    let permissions = fs::metadata(target)?.permissions();
    fs::set_permissions(
        target,
        fs::Permissions::from_mode(permissions.mode() | 0o100),
    )
}

#[cfg(target_os = "macos")]
fn copy_metadata(source: &File, target: &Path) -> io::Result<()> {
    use rustix::fs::CopyfileFlags;

    let target = File::options().write(true).create_new(true).open(target)?;
    let state = rustix::fs::copyfile_state_alloc()?;
    // SAFETY: both descriptors and the allocated state outlive the call. ALL
    // preserves quarantine, ACLs and signing-related extended metadata.
    let copied = unsafe { rustix::fs::fcopyfile(source, &target, state, CopyfileFlags::ALL) };
    // SAFETY: the state has one owner and is freed even when copying fails.
    let freed = unsafe { rustix::fs::copyfile_state_free(state) };
    copied?;
    freed?;
    Ok(())
}

#[cfg(windows)]
pub(super) fn copy(source: &Path, target: &Path) -> io::Result<()> {
    // CopyFile preserves alternate streams, including Zone.Identifier. A new
    // image is created only once per content identity, never on ordinary starts.
    fs::copy(source, target).map(|_| ())
}

#[cfg(unix)]
pub(super) fn stamp(source: &Path, key: &mut Vec<u8>) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::metadata(source)?;
    for n in [metadata.dev(), metadata.ino(), metadata.size()] {
        key.extend(n.to_le_bytes());
    }
    for n in [
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    ] {
        key.extend(n.to_le_bytes());
    }
    // Birth time (statx on Linux, st_birthtime on macOS) tells apart a file
    // replaced at a recycled inode within one tick of a coarse-timestamp
    // filesystem.
    if let Ok(born) = metadata.created()
        && let Ok(born) = born.duration_since(std::time::UNIX_EPOCH)
    {
        key.extend(born.as_nanos().to_le_bytes());
    }
    Ok(())
}

#[cfg(windows)]
pub(super) fn stamp(source: &Path, key: &mut Vec<u8>) -> io::Result<()> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_BASIC_INFO, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FileBasicInfo, FileIdInfo,
        GetFileInformationByHandleEx,
    };

    let source = File::options()
        .access_mode(FILE_READ_ATTRIBUTES)
        .open(source)?;
    let mut basic = FILE_BASIC_INFO::default();
    let mut identity = FILE_ID_INFO::default();
    // SAFETY: the buffers have the layouts and lengths required by their info classes;
    // the source handle remains owned throughout both queries.
    unsafe {
        if GetFileInformationByHandleEx(
            source.as_raw_handle(),
            FileBasicInfo,
            (&raw mut basic).cast(),
            u32::try_from(size_of::<FILE_BASIC_INFO>()).expect("native structure fits DWORD"),
        ) == 0
            || GetFileInformationByHandleEx(
                source.as_raw_handle(),
                FileIdInfo,
                (&raw mut identity).cast(),
                u32::try_from(size_of::<FILE_ID_INFO>()).expect("native structure fits DWORD"),
            ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    key.extend(basic.CreationTime.to_le_bytes());
    key.extend(basic.LastWriteTime.to_le_bytes());
    key.extend(basic.ChangeTime.to_le_bytes());
    key.extend(identity.VolumeSerialNumber.to_le_bytes());
    key.extend(identity.FileId.Identifier);
    Ok(())
}

/// Installation needs crash consistency, not durability: a lost image or
/// receipt is installed again on the next start. Apple's barrier orders the
/// image before its name without a drive-cache flush per file.
pub(super) fn order(file: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    return crate::fs::sync_data(file);
    #[cfg(not(target_vendor = "apple"))]
    file.sync_all()
}

/// Publishes an ordered file so its name never outlives its contents.
pub(super) fn publish(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        crate::fs::replace(from, to)?;
        crate::fs::order_dir(to.parent().ok_or(io::ErrorKind::InvalidInput)?)
    }
    #[cfg(not(target_vendor = "apple"))]
    crate::fs::publish(from, to)
}

pub(super) fn flush(image: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return order(&File::open(image)?);
    #[cfg(windows)]
    {
        // FlushFileBuffers requires write access; CopyFile retains the source's
        // read-only attribute. Change only our unpublished snapshot temporarily.
        let permissions = fs::metadata(image)?.permissions();
        if permissions.readonly() {
            let mut writable = permissions.clone();
            #[allow(
                clippy::permissions_set_readonly_false,
                reason = "Windows only clears FILE_ATTRIBUTE_READONLY, without changing the ACL"
            )]
            writable.set_readonly(false);
            fs::set_permissions(image, writable)?;
        }
        let flushed = File::options()
            .write(true)
            .open(image)
            .and_then(|file| file.sync_all());
        if permissions.readonly() {
            fs::set_permissions(image, permissions)?;
        }
        flushed
    }
}

pub(super) fn discard(image: &Path) -> io::Result<()> {
    fs::remove_file(image)
}

/// Windows cannot unlink a mapped PE image. Retire its launch name without
/// disturbing the process; a later installation reclaims it after unmapping.
pub(super) fn retire(image: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return discard(image);
    #[cfg(windows)]
    match discard(image) {
        Err(error) if matches!(error.raw_os_error(), Some(5 | 32)) => {
            fs::rename(image, image.with_file_name(".retired"))
        }
        result => result,
    }
}

/// Best effort: a retired image another process still maps is reclaimed by
/// a later installation, and a failure here never blocks this one.
pub(super) fn collect(dir: &Path) {
    #[cfg(unix)]
    let _ = dir;
    #[cfg(windows)]
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            #[cfg(not(test))]
            let result = discard(&path.join(".retired"));
            #[cfg(test)]
            let result =
                if std::env::var("A_TEST_IMAGE_HANDLE_PROBE").as_deref() == Ok("posix-file") {
                    probe_discard(&path.join(".retired"))
                } else {
                    discard(&path.join(".retired"))
                };
            #[cfg(test)]
            if std::env::var_os("A_TEST_IMAGE_HANDLE_PROBE").is_some() {
                eprintln!("collect {path:?}: {result:?}");
            }
            let _ = result;
        }
    }
}

#[cfg(all(test, windows))]
fn probe_discard(image: &Path) -> io::Result<()> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX,
        FILE_FLAG_OPEN_REPARSE_POINT, FileDispositionInfoEx, SetFileInformationByHandle,
    };
    let file = File::options()
        .access_mode(DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(image)?;
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    // SAFETY: the owned handle and correctly sized info structure outlive the call.
    crate::windows::ok(unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfoEx,
            (&raw const info).cast(),
            u32::try_from(size_of::<FILE_DISPOSITION_INFO_EX>()).expect("fixed structure size"),
        )
    })
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    #[test]
    fn installed_images_preserve_quarantine_and_the_code_signature() {
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::copy(std::env::current_exe().unwrap(), &source).unwrap();
        assert!(
            Command::new("/usr/bin/codesign")
                .args(["--force", "--sign", "-"])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        let quarantine = "0081;65000000;a-install-test;";
        assert!(
            Command::new("/usr/bin/xattr")
                .args(["-w", "com.apple.quarantine", quarantine])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        let runtime = super::super::Runtime::new(dir.path().join("runtime"));
        let installed = runtime.stage(&source).unwrap();
        let fallback = dir.path().join("fallback");
        super::copy_metadata(&std::fs::File::open(&source).unwrap(), &fallback).unwrap();
        // A clone carries the attribute verbatim. copyfile may restamp the
        // time and agent fields, but must keep the flags that make
        // Gatekeeper assess the image.
        for (image, exact) in [(&installed.exe, true), (&fallback, false)] {
            let output = Command::new("/usr/bin/xattr")
                .args(["-p", "com.apple.quarantine"])
                .arg(image)
                .output()
                .unwrap();
            assert!(output.status.success());
            let found = String::from_utf8(output.stdout).unwrap();
            if exact {
                assert_eq!(found.trim(), quarantine);
            } else {
                assert_eq!(found.split(';').next(), quarantine.split(';').next());
            }
            assert!(
                Command::new("/usr/bin/codesign")
                    .arg("--verify")
                    .arg(image)
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }

    #[cfg(windows)]
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "temporary native lifetime experiment"
    )]
    fn retiring_a_running_image_allows_updates_and_reclamation_after_exit() {
        use std::io::{BufRead, Read, Write};
        use std::os::windows::io::AsHandle;
        use std::os::windows::process::ChildExt;
        use std::process::{Command, Stdio};

        const CHILD: &str = "A_TEST_INSTALLED_IMAGE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            println!("image ready");
            std::io::stdout().flush().unwrap();
            std::io::stdin().read_exact(&mut [0]).unwrap();
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let build = dir.path().join("build");
        std::fs::create_dir(&build).unwrap();
        let exe = build.join(super::super::EXE);
        super::copy(&std::env::current_exe().unwrap(), &exe).unwrap();
        let mode = std::env::var("A_TEST_IMAGE_HANDLE_PROBE").unwrap_or_else(|_| "held".into());
        let mut image_view = matches!(mode.as_str(), "mapped" | "unmapped" | "unmapped-held")
            .then(|| ImageMapping::new(&exe));
        let mut image_reader = matches!(mode.as_str(), "file" | "file-dropped" | "posix-file")
            .then(|| std::fs::File::open(&exe).unwrap());
        let mut child = Some(Command::new(&exe)
            .args([
                "--exact",
                "runtime::image::tests::retiring_a_running_image_allows_updates_and_reclamation_after_exit",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap());
        let retained = match mode.as_str() {
            "duplicate" => Some(
                child
                    .as_ref()
                    .unwrap()
                    .as_handle()
                    .try_clone_to_owned()
                    .unwrap(),
            ),
            "thread" => Some(
                child
                    .as_ref()
                    .unwrap()
                    .main_thread_handle()
                    .try_clone_to_owned()
                    .unwrap(),
            ),
            _ => None,
        };
        let result = (|| {
            let mut output =
                std::io::BufReader::new(child.as_mut().unwrap().stdout.take().unwrap());
            assert!(
                (&mut output)
                    .lines()
                    .any(|line| line.unwrap().ends_with("image ready"))
            );
            super::retire(&exe)?;
            assert!(!exe.exists());
            super::collect(dir.path());
            assert!(child.as_mut().unwrap().try_wait()?.is_none());
            child
                .as_mut()
                .unwrap()
                .stdin
                .take()
                .unwrap()
                .write_all(&[0])?;
            assert!(child.as_mut().unwrap().wait()?.success());
            if mode != "held" && mode != "unmapped-held" {
                drop(child.take());
            }
            if matches!(mode.as_str(), "unmapped" | "unmapped-held") {
                drop(image_view.take());
            }
            eprintln!("image_view_owned={}", image_view.is_some());
            if mode == "file-dropped" {
                drop(image_reader.take());
            }
            eprintln!(
                "before collection: mode={mode} image_reader={} readonly={:?}",
                image_reader.is_some(),
                std::fs::metadata(build.join(".retired")).map(|m| m.permissions().readonly())
            );
            super::collect(dir.path());
            eprintln!(
                "probe mode={mode} child_owned={} extra_handle_owned={} entries={:?}",
                child.is_some(),
                retained.is_some(),
                std::fs::read_dir(&build)
                    .unwrap()
                    .map(|e| e.unwrap().file_name())
                    .collect::<Vec<_>>()
            );
            std::fs::remove_dir(&build)?;
            assert!(!build.exists());
            Ok::<_, std::io::Error>(())
        })();
        if let Some(child) = &mut child {
            let _ = child.kill();
            let _ = child.wait();
        }
        drop(child);
        drop(retained);
        drop(image_reader);
        drop(image_view);
        result.unwrap();
    }

    #[cfg(windows)]
    struct ImageMapping(windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS);

    #[cfg(windows)]
    impl ImageMapping {
        fn new(path: &std::path::Path) -> Self {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::Memory::{
                CreateFileMappingW, FILE_MAP_READ, MapViewOfFile, PAGE_READONLY, SEC_IMAGE,
            };
            let file = std::fs::File::open(path).unwrap();
            // SAFETY: the file is owned through creation; null security/name pointers are allowed.
            let mapping = crate::windows::owned(unsafe {
                CreateFileMappingW(
                    file.as_raw_handle(),
                    std::ptr::null(),
                    PAGE_READONLY | SEC_IMAGE,
                    0,
                    0,
                    std::ptr::null(),
                )
            })
            .unwrap();
            // SAFETY: the mapping is live; a zero length maps the complete file.
            let view = unsafe { MapViewOfFile(mapping.as_raw_handle(), FILE_MAP_READ, 0, 0, 0) };
            assert!(!view.Value.is_null(), "{}", std::io::Error::last_os_error());
            Self(view)
        }
    }

    #[cfg(windows)]
    impl Drop for ImageMapping {
        fn drop(&mut self) {
            // SAFETY: this view has one owner and has not been unmapped.
            unsafe { windows_sys::Win32::System::Memory::UnmapViewOfFile(self.0) };
        }
    }
}
