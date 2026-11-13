//! `log.db`: the append-only note log and its single writer thread with group commit (SPEC §5.3).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

use rusqlite::{Connection, params};
use tokio::sync::oneshot;

use crate::Hash;

const QUEUE: usize = 4096;
const MAX_BATCH: usize = 512;

// Tables arrive with the code that uses them; the full list is SPEC §5.2.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS blobs (id BLOB PRIMARY KEY, len INTEGER, data BLOB) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS notes (id BLOB PRIMARY KEY, gseq INTEGER UNIQUE, author TEXT, session INTEGER,
                                  rank INTEGER, seq INTEGER, kind INTEGER, body BLOB,
                                  posted_at INTEGER) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS note_writes (atom TEXT, note BLOB, PRIMARY KEY (atom, note)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS note_reads  (atom TEXT, hash BLOB, note BLOB, PRIMARY KEY (atom, note)) WITHOUT ROWID;
";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Durability {
    /// Survives a process crash; may lose the last batch on power loss.
    #[default]
    Normal,
    Full,
}

pub struct NewNote {
    pub id: Hash,
    pub author: String,
    pub session: u64,
    pub rank: u64,
    pub seq: u64,
    pub kind: u8,
    pub body: Vec<u8>,
    pub blobs: Vec<NewBlob>,
    /// Write footprint, as atom key text.
    pub writes: Vec<String>,
    /// Read footprint: atom key text and the hash that was read.
    pub reads: Vec<(String, Hash)>,
}

/// A content blob, already zstd-compressed by the caller so the writer thread only does I/O.
pub struct NewBlob {
    pub id: Hash,
    pub len: u64,
    pub zstd: Vec<u8>,
}

impl NewBlob {
    pub fn of(bytes: &[u8]) -> Self {
        NewBlob { id: Hash::of(bytes), len: bytes.len() as u64, zstd: crate::blob::compress(bytes) }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum AppendError {
    #[error("log writer queue is full")]
    Busy,
    #[error("log writer has stopped")]
    Closed,
    #[error("log write failed: {0}")]
    Db(String),
}

struct Req {
    note: NewNote,
    ack: oneshot::Sender<Result<u64, AppendError>>,
}

/// Handle to the writer thread. Cloning is cheap; the thread exits when the last handle drops.
#[derive(Clone)]
pub struct LogWriter {
    tx: SyncSender<Req>,
    last_gseq: Arc<AtomicU64>,
}

/// Creates `log.db` with its tables if needed, so readers can attach it before any daemon ran.
pub fn create(path: &Path) -> rusqlite::Result<()> {
    let conn = Connection::open(path)?;
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
    conn.execute_batch(SCHEMA)
}

impl LogWriter {
    pub fn open(path: &Path, durability: Durability) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
        let sync = match durability {
            Durability::Normal => "NORMAL",
            Durability::Full => "FULL",
        };
        conn.pragma_update(None, "synchronous", sync)?;
        // Checkpointing inside a commit costs the writer an fsync and shows up in post p99.
        // A separate thread checkpoints instead (see `checkpoint_loop`).
        conn.pragma_update(None, "wal_autocheckpoint", 0)?;
        conn.busy_timeout(std::time::Duration::from_secs(1))?;
        conn.execute_batch(SCHEMA)?;
        let last: u64 = conn.query_row("SELECT COALESCE(MAX(gseq), 0) FROM notes", [], |r| r.get(0))?;

        let last_gseq = Arc::new(AtomicU64::new(last));
        let (tx, rx) = sync_channel(QUEUE);
        let shared = last_gseq.clone();
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        let wal = std::path::PathBuf::from(wal);
        std::thread::Builder::new()
            .name("kintrel-log-writer".into())
            .spawn(move || run(conn, rx, shared, wal))
            .expect("spawn log writer thread");
        let checkpointer = Connection::open(path)?;
        let watch = Arc::downgrade(&last_gseq);
        std::thread::Builder::new()
            .name("kintrel-log-checkpoint".into())
            .spawn(move || checkpoint_loop(checkpointer, watch))
            .expect("spawn log checkpoint thread");
        Ok(LogWriter { tx, last_gseq })
    }

    /// Appends a note and resolves with its `gseq` once the batch holding it has committed.
    /// Appending a note id that is already in the log returns the existing `gseq`.
    pub async fn append(&self, note: NewNote) -> Result<u64, AppendError> {
        let (ack, rx) = oneshot::channel();
        self.tx.try_send(Req { note, ack }).map_err(|e| match e {
            TrySendError::Full(_) => AppendError::Busy,
            TrySendError::Disconnected(_) => AppendError::Closed,
        })?;
        rx.await.map_err(|_| AppendError::Closed)?
    }

    pub fn last_gseq(&self) -> u64 {
        self.last_gseq.load(Ordering::Acquire)
    }
}

/// Above this size the writer itself resets the WAL after acknowledging a batch.
const WAL_LIMIT: u64 = 64 << 20;

/// Copies the WAL into the database once a second while there are new notes. PASSIVE never
/// waits for the writer, but it cannot reset the WAL while posts keep arriving; the writer does
/// that (see `run`). Exits once every `LogWriter` and the writer thread are gone.
fn checkpoint_loop(conn: Connection, last_gseq: std::sync::Weak<AtomicU64>) {
    let mut done = 0;
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let Some(last) = last_gseq.upgrade() else { return };
        let now = last.load(Ordering::Acquire);
        drop(last);
        if now != done && conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(())).is_ok() {
            done = now;
        }
    }
}

fn run(mut conn: Connection, rx: Receiver<Req>, last_gseq: Arc<AtomicU64>, wal: std::path::PathBuf) {
    let mut gseq = last_gseq.load(Ordering::Acquire);
    while let Ok(first) = rx.recv() {
        // No artificial wait: requests that queue up during this commit form the next batch,
        // so a lone poster never pays for batching.
        let mut batch = vec![first];
        batch.extend(rx.try_iter().take(MAX_BATCH - 1));
        match write_batch(&mut conn, &batch, gseq) {
            Ok((seqs, next)) => {
                gseq = next;
                last_gseq.store(gseq, Ordering::Release);
                for (req, s) in batch.into_iter().zip(seqs) {
                    let _ = req.ack.send(Ok(s));
                }
            }
            Err(e) => {
                let msg = e.to_string();
                for req in batch {
                    let _ = req.ack.send(Err(AppendError::Db(msg.clone())));
                }
            }
        }
        // Sustained load: the background PASSIVE checkpoints have copied most frames, so this
        // mostly just resets the file. It delays the next batch, and only past WAL_LIMIT.
        if std::fs::metadata(&wal).is_ok_and(|m| m.len() > WAL_LIMIT) {
            let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        }
    }
}

fn write_batch(conn: &mut Connection, batch: &[Req], mut gseq: u64) -> rusqlite::Result<(Vec<u64>, u64)> {
    let now = crate::now_ms() as i64;
    let tx = conn.transaction()?;
    let mut out = Vec::with_capacity(batch.len());
    {
        let mut blob = tx.prepare_cached("INSERT OR IGNORE INTO blobs (id, len, data) VALUES (?1, ?2, ?3)")?;
        let mut note = tx.prepare_cached(
            "INSERT OR IGNORE INTO notes (id, gseq, author, session, rank, seq, kind, body, posted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        let mut existing = tx.prepare_cached("SELECT gseq FROM notes WHERE id = ?1")?;
        let mut write = tx.prepare_cached("INSERT OR IGNORE INTO note_writes (atom, note) VALUES (?1, ?2)")?;
        let mut read = tx.prepare_cached("INSERT OR IGNORE INTO note_reads (atom, hash, note) VALUES (?1, ?2, ?3)")?;
        for Req { note: n, .. } in batch {
            for b in &n.blobs {
                blob.execute(params![&b.id.0[..], b.len, b.zstd])?;
            }
            let inserted = note.execute(params![
                &n.id.0[..], gseq + 1, n.author, n.session, n.rank, n.seq, n.kind, n.body, now
            ])?;
            if inserted == 1 {
                for atom in &n.writes {
                    write.execute(params![atom, &n.id.0[..]])?;
                }
                for (atom, hash) in &n.reads {
                    read.execute(params![atom, &hash.0[..], &n.id.0[..]])?;
                }
                gseq += 1;
                out.push(gseq);
            } else {
                out.push(existing.query_row([&n.id.0[..]], |r| r.get(0))?);
            }
        }
    }
    tx.commit()?;
    Ok((out, gseq))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(i: u64) -> NewNote {
        let body = format!("note {i}").into_bytes();
        NewNote {
            id: Hash::of(&body),
            author: "test".into(),
            session: 1,
            rank: 1,
            seq: i,
            kind: 0,
            blobs: vec![NewBlob { id: Hash::of(b"same content"), len: 12, zstd: b"x".to_vec() }],
            writes: vec![format!("a.rs::fn f{i}")],
            reads: vec![("a.rs".into(), Hash::of(b"r"))],
            body,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn gseq_is_gapless_and_appends_are_idempotent() {
        let dir = std::env::temp_dir().join(format!("kintrel-log-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = LogWriter::open(&dir.join("log.db"), Durability::Normal).unwrap();

        let tasks: Vec<_> = (1..=200).map(|i| tokio::spawn({
            let log = log.clone();
            async move { log.append(note(i)).await.unwrap() }
        })).collect();
        let mut seqs = Vec::new();
        for t in tasks {
            seqs.push(t.await.unwrap());
        }
        seqs.sort();
        assert_eq!(seqs, (1..=200).collect::<Vec<_>>());
        assert_eq!(log.last_gseq(), 200);

        let again = log.append(note(7)).await.unwrap();
        assert!((1..=200).contains(&again));
        assert_eq!(log.last_gseq(), 200);

        drop(log);
        // Reopening resumes from the stored maximum.
        let log = LogWriter::open(&dir.join("log.db"), Durability::Normal).unwrap();
        assert_eq!(log.append(note(201)).await.unwrap(), 201);
        std::fs::remove_dir_all(&dir).ok();
    }
}
