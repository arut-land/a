//! Best-effort native transport for the logger's already sealed JSON records.
//! There is no worker, timer or retry queue here; the bounded logger owns admission.

/// Native delivery supplements the bounded local log. An explicit `A_HOME`
/// keeps diagnostics entirely local, including when a native collector is running.
pub struct Native(Option<backend::Sink>);

impl Native {
    pub fn for_home(home: &crate::Home) -> Self {
        Self::new(home.is_isolated())
    }

    fn new(isolated: bool) -> Self {
        Self((!isolated).then(backend::Sink::new).flatten())
    }

    /// Transport only: callers must admit records through their sealed vocabulary.
    /// Native failures never prevent the readable local copy from being written.
    pub fn write(&self, line: &[u8]) {
        if let Some(sink) = &self.0
            && line.len() <= 4096
            && let Ok(text) = std::str::from_utf8(line)
        {
            sink.write(text.trim_end_matches('\n'));
        }
    }
}

#[cfg(target_os = "linux")]
mod backend {
    use std::os::unix::net::UnixDatagram;

    pub(super) struct Sink(UnixDatagram);
    impl Sink {
        pub(super) fn new() -> Option<Self> {
            Self::connect("/run/systemd/journal/socket")
        }

        fn connect(path: impl AsRef<std::path::Path>) -> Option<Self> {
            let socket = UnixDatagram::unbound().ok()?;
            socket.set_nonblocking(true).ok()?;
            socket.connect(path).ok()?;
            Some(Self(socket))
        }

        pub(super) fn write(&self, text: &str) {
            // JSON escapes embedded newlines. One datagram is one journal entry.
            const PREFIX: &[u8] = b"SYSLOG_IDENTIFIER=a\nPRIORITY=5\nMESSAGE=";
            let mut bytes = [0; 4096 + PREFIX.len() + 1];
            bytes[..PREFIX.len()].copy_from_slice(PREFIX);
            let end = PREFIX.len() + text.len();
            bytes[PREFIX.len()..end].copy_from_slice(text.as_bytes());
            bytes[end] = b'\n';
            let _ = self.0.send(&bytes[..=end]);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        #[cfg_attr(miri, ignore = "Miri does not support AF_UNIX sockets")]
        fn journal_delivery_is_atomic_and_never_waits_for_a_collector() {
            let path = std::env::temp_dir().join(format!("a-journal-{}", std::process::id()));
            let receiver = UnixDatagram::bind(&path).unwrap();
            let sender = Sink::connect(&path).unwrap();
            receiver.set_nonblocking(true).unwrap();
            sender.write(r#"{"fact":"registry_rejected"}"#);
            let mut bytes = [0; 4096];
            let len = receiver.recv(&mut bytes).unwrap();
            assert_eq!(
                &bytes[..len],
                b"SYSLOG_IDENTIFIER=a\nPRIORITY=5\nMESSAGE={\"fact\":\"registry_rejected\"}\n"
            );
            for _ in 0..1000 {
                sender.write("{}");
            }
            drop(receiver);
            sender.write("{}");
            std::fs::remove_file(path).unwrap();
        }
    }
}

#[cfg(target_os = "macos")]
mod backend {
    pub(super) struct Sink(oslog::OsLog);
    impl Sink {
        // The backend constructor is fallible on the other platforms.
        #[allow(clippy::unnecessary_wraps)]
        pub(super) fn new() -> Option<Self> {
            Some(Self(oslog::OsLog::new("land.arut.a", "daemon")))
        }
        pub(super) fn write(&self, text: &str) {
            if self.0.level_is_enabled(oslog::Level::Default) {
                self.0.default(text);
            }
        }
    }
}

#[cfg(windows)]
mod backend {
    tracelogging::define_provider!(PROVIDER, "land.arut.a.daemon");

    // Only daemon installation opens the provider. Reference counting also lets
    // native-sink tests coexist without registering a provider twice.
    static USERS: std::sync::Mutex<usize> = std::sync::Mutex::new(0);
    pub(super) struct Sink;
    impl Sink {
        pub(super) fn new() -> Option<Self> {
            let mut users = USERS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *users == 0 {
                // SAFETY: serialized registration; the last Sink unregisters.
                if unsafe { PROVIDER.register() } != 0 {
                    return None;
                }
            }
            *users += 1;
            Some(Self)
        }
        // This borrow keeps the provider registered while the static macro writes.
        #[allow(clippy::unused_self)]
        pub(super) fn write(&self, text: &str) {
            tracelogging::write_event!(PROVIDER, "Fact", level(Informational), str8("Json", text));
        }
    }
    impl Drop for Sink {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_spawn_preserves_default_and_explicit_home_modes() {
        let root = std::env::temp_dir().join("a-native-home-mode");
        let native = crate::Home {
            root: root.clone(),
            isolated: false,
        };
        let isolated = crate::Home::from(root.clone());
        for (home, expected) in [(native, None), (isolated, Some(root.as_os_str()))] {
            let mut command = std::process::Command::new("unused");
            command.env("A_HOME", "another-instance");
            home.daemon_env(&mut command);
            let value = command
                .get_envs()
                .find(|(key, _)| *key == "A_HOME")
                .unwrap()
                .1;
            assert_eq!(value, expected);
        }
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn native_guards_can_overlap_and_reopen_without_emitting_a_record() {
        let home = crate::Home {
            root: std::env::temp_dir().join("a-native-guard"),
            isolated: false,
        };
        let first = Native::for_home(&home);
        let second = Native::for_home(&home);
        assert!(first.0.is_some() && second.0.is_some());
        drop(first);
        drop(second);
        assert!(Native::for_home(&home).0.is_some());
    }

    #[test]
    fn isolated_diagnostics_never_open_a_native_sink() {
        let home = crate::Home::from(std::env::temp_dir().join("a-native-isolation"));
        assert!(Native::for_home(&home).0.is_none());
    }
}
