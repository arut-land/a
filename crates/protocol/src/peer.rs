//! Who is on the other side of a connection: the machine it is on, and
//! what the daemon lets it do.

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

/// What a connection may do. The daemon decides it from how the connection
/// came; a client never says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Owner,
    /// Read-only: its mutations are rejected.
    Viewer,
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
}
