//! Kintrel core: ids, encoding, storage, trees and (later) notes and the combiner. See `docs/SPEC.md`.

pub mod blob;
pub mod combine;
pub mod board;
pub mod encode;
pub mod id;
pub mod log;
pub mod merge;
pub mod note;
pub mod os;
pub mod path;
pub mod proto;
pub mod state;
pub mod tier2;
pub mod tier3;
pub mod metrics;
pub mod tree;
pub mod worktree;

pub use id::Hash;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Stored data that fails to decode or is missing: a bug or a damaged database.
    #[error("corrupt store: {0}")]
    Corrupt(String),
    /// Input the caller can fix.
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
