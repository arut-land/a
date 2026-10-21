use std::fmt;

/// A host's OS and CPU, normalized for comparing a remote host to this
/// build. This does not identify its libc or other ABI requirements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Platform {
    os: &'static str,
    arch: &'static str,
}

impl Platform {
    pub const CURRENT: Self = Self {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
    };

    /// Recognizes `uname -sm`; unknown hosts cannot receive this build.
    pub fn from_uname(uname: &str) -> Option<Self> {
        let mut words = uname.split_whitespace();
        let os = match words.next()? {
            "Darwin" => "macos",
            "Linux" => "linux",
            "FreeBSD" => "freebsd",
            _ => return None,
        };
        let arch = match words.next()? {
            "arm64" | "aarch64" => "aarch64",
            "x86_64" | "amd64" => "x86_64",
            _ => return None,
        };
        if words.next().is_some() {
            return None;
        }
        Some(Self { os, arch })
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.os, self.arch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_aliases_share_one_identity() {
        for (uname, os) in [
            ("Darwin", "macos"),
            ("Linux", "linux"),
            ("FreeBSD", "freebsd"),
        ] {
            for (aliases, arch) in [
                (["arm64", "aarch64"], "aarch64"),
                (["x86_64", "amd64"], "x86_64"),
            ] {
                for alias in aliases {
                    let host = Platform::from_uname(&format!("  {uname}\t{alias}\n")).unwrap();
                    assert_eq!(host, Platform { os, arch });
                    assert_eq!(host.to_string(), format!("{os} {arch}"));
                }
            }
        }
    }

    #[test]
    fn unknown_or_incomplete_hosts_have_no_identity() {
        for uname in [
            "",
            "Linux",
            "Linux riscv64",
            "Plan9 amd64",
            "Linux x86_64 extra",
        ] {
            assert_eq!(Platform::from_uname(uname), None, "{uname:?}");
        }
    }

    #[test]
    fn either_platform_component_distinguishes_hosts() {
        let host = Platform::from_uname("Linux aarch64").unwrap();
        for other in ["Darwin aarch64", "Linux x86_64"] {
            assert_ne!(Some(host), Platform::from_uname(other));
        }
    }
}
