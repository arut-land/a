//! A host to reach over ssh, checked where it enters (argv, a variable)
//! so that nothing past it can hand ssh an option.

use std::ffi::OsStr;
use std::fmt;
use std::str::FromStr;

/// A host as `ssh` takes one: a `Host` alias from its configuration,
/// `[user@]name` or an `ssh://` URI. Never one that `ssh` would read as
/// an option, as `-oProxyCommand=…` would be.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Host(String);

impl Host {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Host {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.as_bytes().first() {
            None => Err("a host is not empty".to_owned()),
            Some(b'-') => Err(format!(
                "{s:?} is not a host: ssh would take it as an option"
            )),
            Some(_) => Ok(Self(s.to_owned())),
        }
    }
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<OsStr> for Host {
    fn as_ref(&self) -> &OsStr {
        self.0.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_is_never_an_ssh_option() {
        for host in ["box", "me@box", "ssh://me@box:22", "box-2"] {
            assert_eq!(host.parse::<Host>().unwrap().as_str(), host);
        }
        for option in ["-oProxyCommand=touch /tmp/x", "-", ""] {
            assert!(option.parse::<Host>().is_err(), "{option:?}");
        }
    }
}
