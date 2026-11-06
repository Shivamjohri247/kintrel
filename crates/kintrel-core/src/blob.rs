//! Blob compression. Blobs are stored zstd-compressed and keyed by the hash of the raw bytes (SPEC §4.1).

use std::cell::RefCell;

/// zstd level 3 with a per-thread reused context: building a fresh one per blob costs ~100 µs.
pub fn compress(data: &[u8]) -> Vec<u8> {
    thread_local! {
        static ZSTD: RefCell<zstd::bulk::Compressor<'static>> =
            RefCell::new(zstd::bulk::Compressor::new(3).expect("zstd level 3 is valid"));
    }
    ZSTD.with_borrow_mut(|c| c.compress(data)).expect("in-memory zstd cannot fail")
}

/// `len` is the stored uncompressed length, used as the exact output capacity.
pub fn decompress(data: &[u8], len: usize) -> std::io::Result<Vec<u8>> {
    zstd::bulk::decompress(data, len)
}
