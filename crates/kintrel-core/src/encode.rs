//! Canonical encoding for hashed values: one format byte, then postcard (SPEC §4.3).
//! Hashed types use only `Vec`, sorted collections and integers, so equal values encode equally.

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{Error, Hash, Result};

pub const FORMAT: u8 = 1;

pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    let mut out = vec![FORMAT];
    postcard::to_io(value, &mut out).expect("postcard into a Vec cannot fail");
    out
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    match bytes.split_first() {
        Some((&FORMAT, rest)) => postcard::from_bytes(rest).map_err(|e| Error::Corrupt(e.to_string())),
        _ => Err(Error::Corrupt("unknown encoding format".into())),
    }
}

pub fn hash<T: Serialize>(value: &T) -> Hash {
    Hash::of(&encode(value))
}
