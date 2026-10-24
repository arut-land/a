use super::{Command, InstallLayout, Locations, PathBuf, Paths, io};
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

fn temporary_dir() -> io::Result<PathBuf> {
    // confstr is independent of an app's inherited TMPDIR/login environment.
    // SAFETY: a null buffer with length zero queries the required allocation.
    let size = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, std::ptr::null_mut(), 0) };
    if size == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut bytes = vec![0u8; size];
    // SAFETY: bytes is writable for the full size supplied to confstr.
    let written = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            bytes.as_mut_ptr().cast(),
            size,
        )
    };
    if written == 0 || written > size {
        return Err(io::Error::other(
            "cannot resolve Darwin user temporary directory",
        ));
    }
    bytes.truncate(written - 1);
    let path = PathBuf::from(OsString::from_vec(bytes));
    if !path.is_absolute() {
        return Err(io::Error::other(
            "Darwin temporary directory is not absolute",
        ));
    }
    Ok(path)
}

pub(super) fn resolve() -> io::Result<Paths> {
    let library = std::env::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))?
        .join("Library");
    let support = library.join("Application Support/dev.a");
    Ok(Paths::new(Locations {
        layout: InstallLayout::Native,
        config: support.join("config"),
        data: support.clone(),
        state: support,
        cache: library.join("Caches/dev.a"),
        logs: library.join("Logs/dev.a"),
        temporary: temporary_dir()?.join("dev.a"),
        runtime: crate::endpoint::runtime_directory(),
    }))
}

pub(super) fn daemon_env(paths: &Paths, command: &mut Command) {
    // The daemon clears its environment; keep a caller-selected home rather
    // than falling back to the passwd entry in the child.
    let home = paths
        .inner
        .data
        .ancestors()
        .nth(3)
        .expect("home/Library/Application Support/dev.a");
    command.env("HOME", home);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_output_and_disposable_cache_have_different_native_locations() {
        let paths = resolve().unwrap();
        assert!(paths.spill_dir().starts_with(paths.state_dir()));
        assert!(
            paths
                .config_dir()
                .ends_with("Library/Application Support/dev.a/config")
        );
        assert!(paths.crash_dir().starts_with(paths.logs_dir()));
        assert!(
            paths
                .state_dir()
                .ends_with("Library/Application Support/dev.a")
        );
        assert!(paths.cache_dir().ends_with("Library/Caches/dev.a"));
        assert!(paths.logs_dir().ends_with("Library/Logs/dev.a"));
        assert_eq!(
            paths.temporary_dir(),
            temporary_dir().unwrap().join("dev.a")
        );
        assert!(!paths.spill_dir().starts_with(paths.cache_dir()));
    }
}
