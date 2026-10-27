//! Optional debug phases, separate from the typed daemon log.

#[cfg(all(windows, feature = "events-etw"))]
pub use enabled::backend::PhaseSession;

/// Emits a phase in an event-enabled build; neither argument is evaluated otherwise.
#[cfg(feature = "events")]
#[macro_export]
macro_rules! phase {
    ($name:expr, $detail:expr $(,)?) => {
        $crate::phase($name, $detail)
    };
}

/// Emits a phase in an event-enabled build; neither argument is evaluated otherwise.
#[cfg(not(feature = "events"))]
#[macro_export]
macro_rules! phase {
    ($name:expr, $detail:expr $(,)?) => {{}};
}

/// `A_PHASE_DIR` selects per-process files in an existing directory. It can
/// also be set at compile time for children with cleared environments.
/// Without either, records go to stderr. Windows builds with `events-etw`
/// send records to an ETW collector instead.
#[cfg(feature = "events")]
#[inline]
pub fn phase(name: &'static str, detail: impl FnOnce() -> String) {
    enabled::phase(name, detail);
}

#[cfg(feature = "events")]
mod enabled {
    use std::time::{SystemTime, UNIX_EPOCH};

    pub(super) fn phase(name: &'static str, detail: impl FnOnce() -> String) {
        let micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        backend::write(&format!(
            "{micros} {:?} {name} {}\n",
            std::thread::current().id(),
            detail()
        ));
    }

    #[cfg(not(all(windows, feature = "events-etw")))]
    mod backend {
        use std::fs::{File, OpenOptions};
        use std::io::Write;
        use std::sync::{LazyLock, Mutex};

        static FILE: LazyLock<Option<Mutex<File>>> = LazyLock::new(|| {
            let directory = std::env::var_os("A_PHASE_DIR")
                .or_else(|| option_env!("A_PHASE_DIR").map(std::ffi::OsString::from))?;
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(
                    std::path::Path::new(&directory).join(format!("{}.phases", std::process::id())),
                )
                .ok()
                .map(Mutex::new)
        });

        pub(super) fn write(line: &str) {
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

    #[cfg(all(windows, feature = "events-etw"))]
    pub(crate) mod backend {
        use std::sync::Mutex;

        // A fixed identity lets a collector start before any producer does.
        tracelogging::define_provider!(
            PROVIDER,
            "land.arut.a.phase",
            id("d8c6d4a1-2894-4b01-9c0a-ae1fd1b2d985")
        );
        static USERS: Mutex<usize> = Mutex::new(0);

        /// Keeps native phase collection registered while an I/O source lives.
        /// The last source unregisters before its library can be unloaded.
        #[derive(Debug)]
        pub struct PhaseSession {
            _private: (),
        }

        impl PhaseSession {
            /// Registers the provider until the returned session drops.
            pub fn register() -> Option<Self> {
                let mut users = USERS.lock().expect("phase users lock poisoned");
                // SAFETY: registration is serialized, the provider is static,
                // and the last session unregisters before releasing its owner.
                if *users == 0 && unsafe { PROVIDER.register() } != 0 {
                    return None;
                }
                *users += 1;
                Some(Self { _private: () })
            }
        }

        impl Drop for PhaseSession {
            fn drop(&mut self) {
                let mut users = USERS
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *users -= 1;
                if *users == 0 {
                    PROVIDER.unregister();
                }
            }
        }

        pub(super) fn write(line: &str) {
            // A phase can be emitted without a source; serialize it with the
            // last session's unregister so a DLL never leaves callbacks behind.
            let users = USERS.lock().expect("phase users lock poisoned");
            if *users != 0 {
                tracelogging::write_event!(
                    PROVIDER,
                    "Phase",
                    level(Informational),
                    str8("Record", line)
                );
            }
        }
    }
}

#[cfg(all(test, not(feature = "events")))]
mod tests {
    #[test]
    fn disabled_phases_never_evaluate_their_arguments() {
        crate::phase!(
            panic!("disabled phase evaluated its name"),
            panic!("disabled phase evaluated its details"),
        );
    }
}
