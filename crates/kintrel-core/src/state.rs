//! `state.db`: trees, combiner-made blobs, refs and the operation log (SPEC §5.2).
//! The combiner owns the only write connection while the daemon runs; `kintrel init` writes
//! before any daemon exists. Readers open their own `State`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, params};

use std::collections::BTreeMap;

use crate::combine::{Conflict, FileState};
use crate::note::Note;
use crate::tree::{Entry, Kind, Tree};
use crate::{Error, Hash, Result, blob, encode};

// Tables arrive with the code that uses them; the full list is SPEC §5.2.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS blobs (id BLOB PRIMARY KEY, len INTEGER, data BLOB) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS trees (id BLOB PRIMARY KEY, data BLOB) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS refs  (name TEXT PRIMARY KEY, root BLOB) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS oplog (id INTEGER PRIMARY KEY, parent INTEGER, head BLOB, promoted BLOB,
                                  reason TEXT, at INTEGER);
CREATE TABLE IF NOT EXISTS note_state (note BLOB PRIMARY KEY, state INTEGER) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS conflicts  (id BLOB PRIMARY KEY, atom TEXT, note BLOB, body BLOB, state INTEGER) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS marks      (name TEXT PRIMARY KEY, value INTEGER) WITHOUT ROWID;
";

/// `marks` row: every note with `gseq` at or below it is decided (promoted, or conflicted for good).
pub const PROMOTED_GSEQ: &str = "promoted_gseq";

/// One note in [`State::log`].
#[derive(Debug, serde::Serialize)]
pub struct LogEntry {
    pub gseq: u64,
    pub id: Hash,
    pub author: String,
    pub session: u64,
    /// When the log took it, in milliseconds since the Unix epoch.
    pub posted_ms: i64,
    /// posted, applied, conflicted, promoted or rejected.
    pub state: &'static str,
    pub paths: Vec<String>,
}

/// `note_state.state` values (SPEC §4.4). A note with no row is Posted.
pub const APPLIED: i64 = 1;
pub const CONFLICTED: i64 = 2;
pub const PROMOTED_NOTE: i64 = 3;
/// Failed the gate by bisection (SPEC §9.3); never folded again.
pub const REJECTED: i64 = 4;
/// `conflicts.state` value for an unresolved conflict.
pub const OPEN: i64 = 0;
/// `conflicts.state` once the losing session redid its change (SPEC §4.6).
pub const RESOLVED: i64 = 1;

pub const HEAD: &str = "head";
pub const PROMOTED: &str = "promoted";

pub struct State {
    conn: Connection,
    /// `log.db` is attached as `log`, so note blobs are readable too.
    with_log: bool,
    /// Decoded trees. Trees are immutable, so entries never go stale.
    // ponytail: per connection, dropped wholesale at TREE_CACHE entries; share or LRU if memory matters.
    trees: RefCell<HashMap<Hash, Arc<Tree>>>,
}

const TREE_CACHE: usize = 4096;

impl State {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(State { conn, with_log: false, trees: RefCell::default() })
    }

    /// A connection that also reads note content from `log.db` (ARCHITECTURE §3.1).
    pub fn open_with_log(path: &Path, log_db: &Path) -> Result<Self> {
        let mut st = Self::open(path)?;
        let log = log_db.to_str().ok_or_else(|| Error::Invalid(format!("non-UTF-8 path {}", log_db.display())))?;
        st.conn.execute("ATTACH DATABASE ?1 AS log", [log])?;
        st.with_log = true;
        Ok(st)
    }

    /// A board's `state.db` with its `log.db` attached.
    pub fn open_board(repo: &Path) -> Result<Self> {
        Self::open_with_log(&crate::board::state_db(repo), &crate::board::log_db(repo))
    }

    /// Runs `f` in one transaction; a bulk import is orders of magnitude faster this way.
    pub fn batch<T>(&mut self, f: impl FnOnce(&State) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN")?;
        match f(self) {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                self.conn.execute_batch("ROLLBACK").ok();
                Err(e)
            }
        }
    }

    pub fn put_blob(&self, data: &[u8]) -> Result<Hash> {
        let id = Hash::of(data);
        self.conn
            .prepare_cached("INSERT OR IGNORE INTO blobs (id, len, data) VALUES (?1, ?2, ?3)")?
            .execute(params![&id.0[..], data.len() as u64, blob::compress(data)])?;
        Ok(id)
    }

    /// A blob that must exist: a missing one means the store is corrupt.
    pub fn blob(&self, id: Hash) -> Result<Vec<u8>> {
        self.get_blob(id)?.ok_or_else(|| Error::Corrupt(format!("missing blob {id}")))
    }

    pub fn get_blob(&self, id: Hash) -> Result<Option<Vec<u8>>> {
        let sql = if self.with_log {
            "SELECT len, data FROM blobs WHERE id = ?1 UNION ALL SELECT len, data FROM log.blobs WHERE id = ?1 LIMIT 1"
        } else {
            "SELECT len, data FROM blobs WHERE id = ?1"
        };
        let row: Option<(u64, Vec<u8>)> = self
            .conn
            .prepare_cached(sql)?
            .query_row([&id.0[..]], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let Some((len, data)) = row else { return Ok(None) };
        let raw = blob::decompress(&data, len as usize).map_err(|e| Error::Corrupt(format!("blob {id}: {e}")))?;
        if Hash::of(&raw) != id {
            return Err(Error::Corrupt(format!("blob {id} does not match its hash")));
        }
        Ok(Some(raw))
    }

    pub fn put_tree(&self, tree: &Tree) -> Result<Hash> {
        let data = encode::encode(tree);
        let id = Hash::of(&data);
        self.conn
            .prepare_cached("INSERT OR IGNORE INTO trees (id, data) VALUES (?1, ?2)")?
            .execute(params![&id.0[..], data])?;
        Ok(id)
    }

    pub fn get_tree(&self, id: Hash) -> Result<Arc<Tree>> {
        if let Some(t) = self.trees.borrow().get(&id) {
            return Ok(t.clone());
        }
        let data: Vec<u8> = self
            .conn
            .prepare_cached("SELECT data FROM trees WHERE id = ?1")?
            .query_row([&id.0[..]], |r| r.get(0))
            .optional()?
            .ok_or_else(|| Error::Corrupt(format!("missing tree {id}")))?;
        let tree: Arc<Tree> = Arc::new(encode::decode(&data)?);
        let mut cache = self.trees.borrow_mut();
        if cache.len() >= TREE_CACHE {
            cache.clear();
        }
        cache.insert(id, tree.clone());
        Ok(tree)
    }

    /// Resolves a `/`-separated path under `root` to its entry kind and id.
    pub fn lookup(&self, root: Hash, path: &str) -> Result<Option<(Kind, Hash)>> {
        let mut cur = (Kind::Dir, root);
        for seg in path.split('/') {
            if cur.0 != Kind::Dir {
                return Ok(None);
            }
            match self.get_tree(cur.1)?.get(seg) {
                Some(e) => cur = (e.kind, e.id),
                None => return Ok(None),
            }
        }
        Ok(Some(cur))
    }

    /// The tree `root` with `changes` applied. Directories left empty disappear.
    pub fn apply(&self, root: Hash, changes: &BTreeMap<String, FileState>) -> Result<Hash> {
        let changes: Vec<(&str, FileState)> = changes.iter().map(|(p, s)| (p.as_str(), *s)).collect();
        match self.apply_at(Some(root), &changes)? {
            Some(id) => Ok(id),
            None => self.put_tree(&Tree::default()),
        }
    }

    fn apply_at(&self, root: Option<Hash>, changes: &[(&str, FileState)]) -> Result<Option<Hash>> {
        let mut entries: BTreeMap<String, Entry> = match root {
            Some(id) => self.get_tree(id)?.entries().iter().map(|e| (e.name.clone(), e.clone())).collect(),
            None => BTreeMap::new(),
        };
        // Per first segment: a change to the entry itself, and changes below it.
        type Split<'a> = (Option<FileState>, Vec<(&'a str, FileState)>);
        let mut by_name: BTreeMap<&str, Split> = BTreeMap::new();
        for &(path, state) in changes {
            match path.split_once('/') {
                None => by_name.entry(path).or_default().0 = Some(state),
                Some((dir, rest)) => by_name.entry(dir).or_default().1.push((rest, state)),
            }
        }
        for (name, (direct, below)) in by_name {
            if let Some(state) = direct {
                match state {
                    Some((kind, id)) => entries.insert(name.to_owned(), Entry { name: name.to_owned(), kind, id }),
                    None => entries.remove(name),
                };
            }
            if !below.is_empty() {
                let sub = entries.get(name).filter(|e| e.kind == Kind::Dir).map(|e| e.id);
                match self.apply_at(sub, &below)? {
                    Some(id) => entries.insert(name.to_owned(), Entry { name: name.to_owned(), kind: Kind::Dir, id }),
                    None => entries.remove(name),
                };
            }
        }
        if entries.is_empty() {
            return Ok(None);
        }
        self.put_tree(&Tree::new(entries.into_values().collect())?).map(Some)
    }

    /// Notes with `gseq` above `after`, in `gseq` order. Needs [`State::open_with_log`].
    pub fn log_notes_after(&self, after: u64) -> Result<Vec<(u64, Hash, Note)>> {
        let mut stmt = self.conn.prepare("SELECT gseq, id, body FROM log.notes WHERE gseq > ?1 ORDER BY gseq")?;
        let rows = stmt.query_map([after], |r| Ok((r.get::<_, u64>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, Vec<u8>>(2)?)))?;
        rows.map(|row| {
            let (gseq, id, body) = row?;
            let id = <[u8; 32]>::try_from(id.as_slice()).map_err(|_| Error::Corrupt("note id".into()))?;
            Ok((gseq, Hash(id), encode::decode(&body)?))
        })
        .collect()
    }

    /// A logged note by full or abbreviated id (at least 8 hex digits). `None` if no note or
    /// more than one matches.
    pub fn find_note(&self, prefix: &str) -> Result<Option<(Hash, Note)>> {
        if prefix.len() < 8 || prefix.len() > 64 || !prefix.len().is_multiple_of(2) || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(None);
        }
        let bytes: Vec<u8> = (0..prefix.len()).step_by(2).map(|i| u8::from_str_radix(&prefix[i..i + 2], 16).expect("hex")).collect();
        let mut stmt = self.conn.prepare_cached("SELECT id, body FROM log.notes WHERE substr(id, 1, ?1) = ?2 LIMIT 2")?;
        let rows: Vec<(Vec<u8>, Vec<u8>)> =
            stmt.query_map(params![bytes.len() as i64, bytes], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
        let [(id, body)] = rows.as_slice() else { return Ok(None) };
        let id = <[u8; 32]>::try_from(id.as_slice()).map_err(|_| Error::Corrupt("note id".into()))?;
        Ok(Some((Hash(id), encode::decode(body)?)))
    }

    /// The highest session id in the log (0 if none). Session ids double as σ and are hashed into
    /// note ids, so a restarted daemon must never reuse one.
    pub fn max_session(&self) -> Result<u64> {
        Ok(self.conn.query_row("SELECT COALESCE(MAX(session), 0) FROM log.notes", [], |r| r.get(0))?)
    }

    /// Notes promoted with `gseq` in `(after, upto]`, as (id, author), in log order. Needs the
    /// log attached.
    pub fn promoted_between(&self, after: u64, upto: u64) -> Result<Vec<(Hash, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT l.id, l.author FROM log.notes l JOIN note_state s ON s.note = l.id AND s.state = ?3
             WHERE l.gseq > ?1 AND l.gseq <= ?2 ORDER BY l.gseq",
        )?;
        let rows = stmt.query_map(params![after, upto, PROMOTED_NOTE], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?)))?;
        rows.map(|row| {
            let (id, author) = row?;
            Ok((Hash(<[u8; 32]>::try_from(id.as_slice()).map_err(|_| Error::Corrupt("note id".into()))?), author))
        })
        .collect()
    }

    pub fn get_mark(&self, name: &str) -> Result<u64> {
        Ok(self
            .conn
            .prepare_cached("SELECT value FROM marks WHERE name = ?1")?
            .query_row([name], |r| r.get(0))
            .optional()?
            .unwrap_or(0))
    }

    /// Records a combiner pass over the `working` notes: their states, their open conflicts and
    /// head (SPEC §5.5). Promotion is separate ([`State::promote_to`]).
    pub fn record_pass(&self, head: Hash, working: &[Hash], applied: &[Hash], conflicts: &[Conflict]) -> Result<()> {
        let mut clear_state = self.conn.prepare_cached("DELETE FROM note_state WHERE note = ?1")?;
        let mut clear_conflicts = self.conn.prepare_cached("DELETE FROM conflicts WHERE note = ?1 AND state = ?2")?;
        for id in working {
            clear_state.execute([&id.0[..]])?;
            clear_conflicts.execute(params![&id.0[..], OPEN])?;
        }
        let applied_state = APPLIED;
        let mut state = self.conn.prepare_cached("INSERT OR REPLACE INTO note_state (note, state) VALUES (?1, ?2)")?;
        for id in applied {
            state.execute(params![&id.0[..], applied_state])?;
        }
        let mut conflict = self
            .conn
            .prepare_cached("INSERT OR IGNORE INTO conflicts (id, atom, note, body, state) VALUES (?1, ?2, ?3, ?4, ?5)")?;
        for c in conflicts {
            state.execute(params![&c.losing.0[..], CONFLICTED])?;
            conflict.execute(params![&c.id().0[..], c.atom.to_string(), &c.losing.0[..], encode::encode(c), OPEN])?;
        }
        // A conflict is resolved once a later note from the losing session that writes the same
        // atom is in head: that is the agent refreshing and redoing its change, which is all a
        // Claude Code agent can do. A resolved row is never cleared, so it stays resolved.
        self.conn.execute(
            "UPDATE conflicts SET state = ?1 WHERE state = ?2 AND EXISTS (
                SELECT 1 FROM log.notes l
                JOIN log.notes r ON r.session = l.session AND r.gseq > l.gseq
                JOIN log.note_writes w ON w.note = r.id AND w.atom = conflicts.atom
                JOIN note_state s ON s.note = r.id AND s.state IN (?3, ?4)
                WHERE l.id = conflicts.note)",
            params![RESOLVED, OPEN, APPLIED, PROMOTED_NOTE],
        )?;
        if self.get_ref(HEAD)? != Some(head) {
            let promoted = self.get_ref(PROMOTED)?.ok_or_else(|| Error::Corrupt("no promoted ref".into()))?;
            self.set_refs(head, promoted, "combine")?;
        }
        Ok(())
    }

    /// Promotes `root`, the head of an earlier pass that folded every note up to `gseq`: those
    /// notes are decided (applied ones become Promoted), and later passes fold only newer notes
    /// on top of `root`. Head stays `head` until the next pass.
    pub fn promote_to(&self, head: Hash, root: Hash, gseq: u64) -> Result<()> {
        self.conn.execute_batch("SAVEPOINT promote")?;
        let r = (|| {
            self.conn.execute("INSERT OR REPLACE INTO marks (name, value) VALUES (?1, ?2)", params![PROMOTED_GSEQ, gseq])?;
            self.conn.execute(
                "UPDATE note_state SET state = ?1 WHERE state = ?2 AND note IN (SELECT id FROM log.notes WHERE gseq <= ?3)",
                params![PROMOTED_NOTE, APPLIED, gseq],
            )?;
            self.set_refs(head, root, "promote")
        })();
        let end = if r.is_ok() { "RELEASE promote" } else { "ROLLBACK TO promote; RELEASE promote" };
        self.conn.execute_batch(end)?;
        r
    }

    /// Every rejected note.
    /// Marks conflicts resolved: their `Resolution` notes are in head.
    /// Resolves each open conflict whose atom lies in one of the paths given with it (the files
    /// the resolving note wrote).
    pub fn resolve_conflicts(&self, resolved: &[(Hash, Vec<&str>)]) -> Result<()> {
        let mut atom = self.conn.prepare_cached("SELECT atom FROM conflicts WHERE id = ?1 AND state = ?2")?;
        let mut stmt = self.conn.prepare_cached("UPDATE conflicts SET state = ?1 WHERE id = ?2")?;
        for (id, paths) in resolved {
            let Some(text) = atom.query_row(params![&id.0[..], OPEN], |r| r.get::<_, String>(0)).optional()? else { continue };
            let key: crate::note::AtomKey = text.parse().map_err(|_| Error::Corrupt("conflict atom".into()))?;
            if paths.contains(&key.path.as_str()) {
                stmt.execute(params![RESOLVED, &id.0[..]])?;
            }
        }
        Ok(())
    }

    pub fn rejected_notes(&self) -> Result<std::collections::HashSet<Hash>> {
        let mut stmt = self.conn.prepare_cached("SELECT note FROM note_state WHERE state = ?1")?;
        let rows = stmt.query_map([REJECTED], |r| r.get::<_, Vec<u8>>(0))?;
        rows.map(|b| <[u8; 32]>::try_from(b?.as_slice()).map(Hash).map_err(|_| Error::Corrupt("note id".into()))).collect()
    }

    pub fn reject(&self, note: Hash) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO note_state (note, state) VALUES (?1, ?2)", params![&note.0[..], REJECTED])?;
        Ok(())
    }

    pub fn open_conflict_count(&self) -> Result<u64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM conflicts WHERE state = ?1", [OPEN], |r| r.get(0))?)
    }

    /// `note_state.state` for a note, `None` if the combiner has not recorded it.
    /// The log, newest first (SPEC §14: provenance is always queryable): at most `limit` notes,
    /// only `author`'s if given. Needs the log attached.
    pub fn log(&self, author: Option<&str>, limit: u32) -> Result<Vec<LogEntry>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT l.gseq, l.id, l.author, l.session, l.posted_at, COALESCE(s.state, 0), l.body
             FROM log.notes l LEFT JOIN note_state s ON s.note = l.id
             WHERE ?1 IS NULL OR l.author = ?1 ORDER BY l.gseq DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![author, limit], |r| {
            Ok((r.get::<_, u64>(0)?, r.get::<_, Vec<u8>>(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get::<_, i64>(5)?, r.get::<_, Vec<u8>>(6)?))
        })?;
        rows.map(|row| {
            let (gseq, id, author, session, posted_ms, state, body) = row?;
            let note: crate::note::Note = encode::decode(&body).map_err(|_| Error::Corrupt("note body".into()))?;
            let state = match state {
                APPLIED => "applied",
                CONFLICTED => "conflicted",
                PROMOTED_NOTE => "promoted",
                REJECTED => "rejected",
                _ => "posted",
            };
            Ok(LogEntry {
                gseq,
                id: Hash(<[u8; 32]>::try_from(id.as_slice()).map_err(|_| Error::Corrupt("note id".into()))?),
                author,
                session,
                posted_ms,
                state,
                paths: note.ops.iter().map(|op| op.path().to_owned()).collect(),
            })
        })
        .collect()
    }

    pub fn note_state(&self, note: Hash) -> Result<Option<i64>> {
        Ok(self
            .conn
            .prepare_cached("SELECT state FROM note_state WHERE note = ?1")?
            .query_row([&note.0[..]], |r| r.get(0))
            .optional()?)
    }

    /// Head, plus each session's latest applied note, read in one snapshot. A view based on this
    /// head was built on exactly those notes (and, through each session's chain, the ones before),
    /// so they become the parents of the view's next note (SPEC §7.2 fold order).
    pub fn head_and_frontier(&self) -> Result<(Hash, Vec<Hash>)> {
        self.conn.execute_batch("BEGIN")?;
        let out = (|| {
            let head = self.get_ref(HEAD)?.ok_or_else(|| Error::Corrupt("no head ref".into()))?;
            let mut stmt = self.conn.prepare_cached(
                "SELECT n.session, n.seq, n.id FROM log.notes n JOIN note_state s ON s.note = n.id WHERE s.state = ?1",
            )?;
            let mut latest: BTreeMap<u64, (u64, Vec<u8>)> = BTreeMap::new();
            for row in stmt.query_map([APPLIED], |r| Ok((r.get::<_, u64>(0)?, r.get::<_, u64>(1)?, r.get::<_, Vec<u8>>(2)?)))? {
                let (session, seq, id) = row?;
                if latest.get(&session).is_none_or(|(s, _)| seq > *s) {
                    latest.insert(session, (seq, id));
                }
            }
            let frontier = latest
                .into_values()
                .map(|(_, id)| <[u8; 32]>::try_from(id.as_slice()).map(Hash).map_err(|_| Error::Corrupt("note id".into())))
                .collect::<Result<Vec<_>>>()?;
            Ok((head, frontier))
        })();
        self.conn.execute_batch("COMMIT")?;
        out
    }

    pub fn open_conflicts(&self) -> Result<Vec<Conflict>> {
        let mut stmt = self.conn.prepare_cached("SELECT body FROM conflicts WHERE state = ?1 ORDER BY atom")?;
        let rows = stmt.query_map([OPEN], |r| r.get::<_, Vec<u8>>(0))?;
        rows.map(|b| encode::decode(&b?)).collect()
    }

    pub fn get_ref(&self, name: &str) -> Result<Option<Hash>> {
        let root: Option<Vec<u8>> = self
            .conn
            .prepare_cached("SELECT root FROM refs WHERE name = ?1")?
            .query_row([name], |r| r.get(0))
            .optional()?;
        root.map(|b| {
            <[u8; 32]>::try_from(b.as_slice()).map(Hash).map_err(|_| Error::Corrupt(format!("ref {name}")))
        })
        .transpose()
    }

    /// Moves head and promoted together and appends the operation-log row (SPEC §5.5).
    pub fn set_refs(&self, head: Hash, promoted: Hash, reason: &str) -> Result<()> {
        self.conn.execute_batch("SAVEPOINT set_refs")?;
        let r = self.write_refs(head, promoted, reason);
        let end = if r.is_ok() { "RELEASE set_refs" } else { "ROLLBACK TO set_refs; RELEASE set_refs" };
        self.conn.execute_batch(end)?;
        r
    }

    fn write_refs(&self, head: Hash, promoted: Hash, reason: &str) -> Result<()> {
        let at = crate::now_ms() as i64;
        let mut set = self.conn.prepare_cached("INSERT OR REPLACE INTO refs (name, root) VALUES (?1, ?2)")?;
        set.execute(params![HEAD, &head.0[..]])?;
        set.execute(params![PROMOTED, &promoted.0[..]])?;
        self.conn.execute(
            "INSERT INTO oplog (parent, head, promoted, reason, at) VALUES ((SELECT MAX(id) FROM oplog), ?1, ?2, ?3, ?4)",
            params![&head.0[..], &promoted.0[..], reason, at],
        )?;
        Ok(())
    }
}
