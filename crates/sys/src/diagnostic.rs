//! Optional debug phases, separate from the typed daemon log.

/// Records a debug phase only in builds with `events`. Details are never
/// constructed otherwise. `A_PHASE_DIR` selects per-process files in an
/// existing directory; without it records go to stderr.
#[inline]
pub fn phase(name: &'static str, detail: impl FnOnce() -> String) {
    #[cfg(feature = "events")]
    enabled::phase(name, detail);
    #[cfg(not(feature = "events"))]
    let _ = (name, detail);
}

#[cfg(feature = "events")]
mod enabled {
    use std::fs::{File, OpenOptions};
    use std::io::Write;
    use std::sync::{LazyLock, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    static FILE: LazyLock<Option<Mutex<File>>> = LazyLock::new(|| {
        let directory = std::env::var_os("A_PHASE_DIR")?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(std::path::Path::new(&directory).join(format!("{}.phases", std::process::id())))
            .ok()
            .map(Mutex::new)
    });

    pub(super) fn phase(name: &'static str, detail: impl FnOnce() -> String) {
        let micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        let line = format!(
            "{micros} {:?} {name} {}\n",
            std::thread::current().id(),
            detail()
        );
        if let Some(file) = &*FILE {
            let _ = file
                .lock()
                .expect("phase file lock poisoned")
                .write_all(line.as_bytes());
        } else {
            let _ = std::io::stderr().lock().write_all(line.as_bytes());
        }
    }
}

#[cfg(all(test, not(feature = "events")))]
mod tests {
    #[test]
    fn disabled_phases_never_construct_their_details() {
        super::phase("disabled", || {
            panic!("disabled phase constructed its details")
        });
    }
}
