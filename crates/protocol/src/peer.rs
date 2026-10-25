//! Who is on the other side of a connection: the machine, and the
//! transport that vouched for the person on it.

use std::ffi::OsStr;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A daemon's stable identity: the first 128 bits of the SHA-256 of its
/// home's long-term X25519 public key, made the first time the home needs
/// one. Two homes on one computer are two machines. Shown as 26 lowercase
/// base32 digits (RFC 4648).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MachineId(pub [u8; 16]);

const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// 128 bits at 5 a digit.
const DIGITS: usize = 26;

impl fmt::Display for MachineId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bits = u128::from_be_bytes(self.0);
        let mut digits = [0; DIGITS];
        for (i, digit) in digits.iter_mut().enumerate() {
            // The last digit holds the lowest three bits, then two zeros.
            let at = match 5 * (DIGITS - 1 - i) {
                0 => bits << 2,
                shift => bits >> (shift - 2),
            };
            *digit = BASE32[at as usize & 31];
        }
        f.write_str(std::str::from_utf8(&digits).map_err(|_| fmt::Error)?)
    }
}

impl fmt::Debug for MachineId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MachineId({self})")
    }
}

impl FromStr for MachineId {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        const INVALID: &str = "a machine id is 26 lowercase base32 digits";
        if s.len() != DIGITS {
            return Err(INVALID);
        }
        let digit = |c: u8| BASE32.iter().position(|&d| d == c).ok_or(INVALID);
        let (last, high) = s.as_bytes().split_last().ok_or(INVALID)?;
        let high = high
            .iter()
            .try_fold(0_u128, |bits, &c| Ok((bits << 5) | digit(c)? as u128))?;
        let last = digit(*last)? as u128;
        // Its two spare bits are zero in the one spelling.
        if last & 0b11 != 0 {
            return Err(INVALID);
        }
        Ok(Self(((high << 3) | (last >> 2)).to_be_bytes()))
    }
}

impl Serialize for MachineId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MachineId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

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

/// How a connection reached the daemon, which says who is on it. Trust is
/// "this user, on a transport that authenticated this user": each way in
/// is one that does, and a new one decides what it proves where the
/// daemon matches on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Via {
    /// This machine's endpoint, where the OS proved the peer is this user
    /// (a socket's peer credentials, a pipe's per-user access list). A
    /// bridge `ssh` runs is local to the daemon it serves.
    Local,
    /// `ssh` to `host`, which authenticated this user there.
    Ssh { host: Host },
}

/// What a connection may do. The daemon decides it from the connection's
/// [`Peer`]; a client never says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Owner,
    /// Read-only: its mutations are rejected.
    Viewer,
}

/// Who is on the other side of a connection: the machine it runs on, and
/// how it came.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub machine: MachineId,
    pub via: Via,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_id_has_one_spelling_and_reads_back() {
        for bytes in [[0; 16], [0xff; 16], *b"0123456789abcdef"] {
            let id = MachineId(bytes);
            let shown = id.to_string();
            assert_eq!(shown.len(), 26);
            assert_eq!(shown.parse(), Ok(id));
        }
        assert_eq!(MachineId([0xff; 16]).to_string(), "7".repeat(25) + "4");
        // Spare bits set, an upper-case digit, a short one.
        for wrong in ["7".repeat(26), "A".repeat(26), "aaaa".into()] {
            assert!(wrong.parse::<MachineId>().is_err(), "{wrong}");
        }
    }

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
