use crate::{Os, Platform};

/// A remote host's layout, independent of the connecting client's platform.
#[derive(Clone, Copy, Debug)]
pub struct RemoteLayout {
    native_runtime: &'static str,
}

const LINUX: &str = r#"case "$XDG_DATA_HOME" in
/*) a_runtime="$XDG_DATA_HOME/a/runtime" ;;
*) [ -n "$HOME" ] || exit 1
a_runtime="$HOME/.local/share/a/runtime" ;;
esac"#;
const MACOS: &str = r#"[ -n "$HOME" ] || exit 1
a_runtime="$HOME/Library/Application Support/dev.a/runtime""#;

impl RemoteLayout {
    pub fn for_platform(platform: Platform) -> Self {
        Self {
            native_runtime: match platform.os() {
                Os::Linux => LINUX,
                Os::Macos => MACOS,
                // This bootstrap speaks POSIX shell, not a native Windows login
                // protocol. An explicitly installed `a` on PATH remains usable.
                Os::Windows | Os::FreeBsd => "exit 127",
            },
        }
    }

    /// Sets `a_runtime` from a known host's environment, including isolation.
    pub fn runtime_script(self) -> String {
        with_override(self.native_runtime)
    }

    /// Selects the host layout inside the existing SSH discovery call.
    pub fn discovery_script() -> String {
        with_override(&format!(
            "case \"$(uname -s)\" in\nLinux)\n{LINUX}\n;;\nDarwin)\n{MACOS}\n;;\n*) exit 127 ;;\nesac"
        ))
    }
}

fn with_override(native: &str) -> String {
    format!(
        r#"if [ "${{A_HOME+x}}" = x ]; then
[ -n "$A_HOME" ] || exit 1
a_runtime="$A_HOME/runtime"
else
{native}
fi"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_client_describes_both_supported_remote_layouts() {
        for (uname, location) in [
            ("Linux x86_64", "$XDG_DATA_HOME/a/runtime"),
            (
                "Darwin arm64",
                "$HOME/Library/Application Support/dev.a/runtime",
            ),
        ] {
            let layout = RemoteLayout::for_platform(Platform::from_uname(uname).unwrap());
            assert!(layout.runtime_script().contains(location));
            assert!(RemoteLayout::discovery_script().contains(layout.native_runtime));
        }
    }
}
