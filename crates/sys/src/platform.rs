use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Macos,
    Linux,
    FreeBsd,
    Windows,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    Aarch64,
    X86_64,
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Macos => "macos",
            Self::Linux => "linux",
            Self::FreeBsd => "freebsd",
            Self::Windows => "windows",
        })
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Aarch64 => "aarch64",
            Self::X86_64 => "x86_64",
        })
    }
}

/// A host's OS and CPU, normalized for comparing a remote host to this
/// build. This does not identify its libc or other ABI requirements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Platform {
    os: Os,
    arch: Arch,
}

impl Platform {
    pub const CURRENT: Self = Self {
        #[cfg(target_os = "macos")]
        os: Os::Macos,
        #[cfg(target_os = "linux")]
        os: Os::Linux,
        #[cfg(target_os = "freebsd")]
        os: Os::FreeBsd,
        #[cfg(windows)]
        os: Os::Windows,
        #[cfg(target_arch = "aarch64")]
        arch: Arch::Aarch64,
        #[cfg(target_arch = "x86_64")]
        arch: Arch::X86_64,
    };

    /// Recognizes `uname -sm`; unknown hosts cannot receive this build.
    pub fn from_uname(uname: &str) -> Option<Self> {
        let mut words = uname.split_whitespace();
        let os = match words.next()? {
            "Darwin" => Os::Macos,
            "Linux" => Os::Linux,
            "FreeBSD" => Os::FreeBsd,
            _ => return None,
        };
        let arch = match words.next()? {
            "arm64" | "aarch64" => Arch::Aarch64,
            "x86_64" | "amd64" => Arch::X86_64,
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
            ("Darwin", Os::Macos),
            ("Linux", Os::Linux),
            ("FreeBSD", Os::FreeBsd),
        ] {
            for (aliases, arch) in [
                (["arm64", "aarch64"], Arch::Aarch64),
                (["x86_64", "amd64"], Arch::X86_64),
            ] {
                for alias in aliases {
                    let host = Platform::from_uname(&format!("  {uname}\t{alias}\n")).unwrap();
                    assert_eq!(host, Platform { os, arch });
                }
            }
        }
    }

    #[test]
    fn platform_labels_keep_their_external_spelling() {
        for (os, name) in [
            (Os::Macos, "macos"),
            (Os::Linux, "linux"),
            (Os::FreeBsd, "freebsd"),
            (Os::Windows, "windows"),
        ] {
            for (arch, cpu) in [(Arch::Aarch64, "aarch64"), (Arch::X86_64, "x86_64")] {
                assert_eq!(Platform { os, arch }.to_string(), format!("{name} {cpu}"));
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
