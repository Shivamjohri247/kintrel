use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// BLAKE3-256. Hex in text formats (JSON), raw bytes in binary ones (postcard).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub fn of(bytes: &[u8]) -> Self {
        Hash(*blake3::hash(bytes).as_bytes())
    }

    /// First 12 hex characters, as shown by the CLI.
    pub fn short(&self) -> String {
        self.to_string()[..12].to_owned()
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.short())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid hash: expected 64 lowercase hex characters")]
pub struct ParseHashError;

impl FromStr for Hash {
    type Err = ParseHashError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.as_bytes();
        if s.len() != 64 {
            return Err(ParseHashError);
        }
        let nibble = |c: u8| match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            _ => Err(ParseHashError),
        };
        let mut out = [0u8; 32];
        for (i, pair) in s.chunks_exact(2).enumerate() {
            out[i] = nibble(pair[0])? << 4 | nibble(pair[1])?;
        }
        Ok(Hash(out))
    }
}

impl Serialize for Hash {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.collect_str(self)
        } else {
            self.0.serialize(s)
        }
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if d.is_human_readable() {
            let s = String::deserialize(d)?;
            s.parse().map_err(serde::de::Error::custom)
        } else {
            <[u8; 32]>::deserialize(d).map(Hash)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let h = Hash::of(b"kintrel");
        let text = h.to_string();
        assert_eq!(text.len(), 64);
        assert_eq!(text.parse::<Hash>().unwrap(), h);
        assert_eq!(serde_json::from_str::<Hash>(&serde_json::to_string(&h).unwrap()).unwrap(), h);
        assert!("ABCD".parse::<Hash>().is_err());
        assert!("g".repeat(64).parse::<Hash>().is_err());
    }
}
