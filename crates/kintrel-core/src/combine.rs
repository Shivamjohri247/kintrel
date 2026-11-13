//! The combiner (SPEC §7): folds notes into head.
//!
//! [`fold`] is pure: head is a function of the promoted root and the set of notes, never of
//! arrival order. Each pass refolds every eligible note from the promoted root.
// ponytail: full refold per pass; the working set is bounded by promotion (step 6). Refold only
// the touched files if passes get slow.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender, SyncSender, sync_channel};
use std::time::{Duration, Instant};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::merge::merge3;
use crate::note::{AtomDetail, AtomKey, Note, Op, atom_details};
use crate::state::{self, State};
use crate::tree::Kind;
use crate::{Error, Hash, Result};

/// Larger files that both sides changed are a conflict rather than a merge: every pass refolds,
/// so merge cost must stay bounded.
// ponytail: fixed cap; cache decided notes across passes (incremental fold) to lift it.
pub const MAX_MERGE: usize = 256 << 10;

/// A path's state in head: a file, or `None` if deleted.
pub type FileState = Option<(Kind, Hash)>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub atom: AtomKey,
    /// The note whose version stays in head (the last one applied to the file), if any.
    pub kept: Option<Hash>,
    /// The note that could not be applied. All of its ops stay out of head (atomicity).
    pub losing: Hash,
}

impl Conflict {
    pub fn id(&self) -> Hash {
        crate::encode::hash(self)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Folded {
    /// Every path that differs from the promoted root.
    pub files: BTreeMap<String, FileState>,
    /// Merged contents created by the fold, not yet stored anywhere.
    pub blobs: BTreeMap<Hash, Vec<u8>>,
    pub applied: Vec<Hash>,
    pub conflicts: Vec<Conflict>,
}

/// What the fold needs from the store.
pub trait Source {
    /// The entry at `path` in the promoted root (directories included).
    fn promoted(&self, path: &str) -> Result<Option<(Kind, Hash)>>;
    fn blob(&self, id: Hash) -> Result<Vec<u8>>;
}

/// Fold order (SPEC §7.2): a topological sort over `parents`, ties broken by
/// `(rank, seq, session, id)`. Parents outside `notes` (promoted or unknown) are ignored.
pub fn fold_order(notes: &[(Hash, Note)]) -> Vec<usize> {
    let index: HashMap<Hash, usize> = notes.iter().enumerate().map(|(i, (id, _))| (*id, i)).collect();
    let mut waiting = vec![0usize; notes.len()];
    let mut children: Vec<Vec<usize>> = vec![vec![]; notes.len()];
    for (i, (_, n)) in notes.iter().enumerate() {
        for p in n.parents.iter().filter_map(|p| index.get(p)) {
            waiting[i] += 1;
            children[*p].push(i);
        }
    }
    let key = |i: usize| {
        let (id, n) = &notes[i];
        Reverse((n.rank, n.seq, n.session, *id, i))
    };
    let mut ready: BinaryHeap<_> = (0..notes.len()).filter(|&i| waiting[i] == 0).map(key).collect();
    let mut out = Vec::with_capacity(notes.len());
    while let Some(Reverse((.., i))) = ready.pop() {
        out.push(i);
        for &c in &children[i] {
            waiting[c] -= 1;
            if waiting[c] == 0 {
                ready.push(key(c));
            }
        }
    }
    // A parent cycle cannot come from honest sessions; its notes are simply never applied.
    out
}

pub fn fold(src: &dyn Source, notes: &[(Hash, Note)]) -> Result<Folded> {
    let mut out = Folded::default();
    let mut last_writer: HashMap<String, Hash> = HashMap::new();
    let mut cache: HashMap<Hash, Vec<u8>> = HashMap::new();

    for i in fold_order(notes) {
        let (id, note) = &notes[i];
        let mut staged: BTreeMap<String, FileState> = BTreeMap::new();
        let mut lost: Vec<(String, AtomKey)> = Vec::new();
        for op in &note.ops {
            let path = op.path();
            // A store error on one note (say, a missing blob) must not stall every later pass:
            // the note conflicts on the file instead.
            let file_level = || (path.to_owned(), AtomKey { path: path.to_owned(), symbol: vec![] });
            let Ok(cur) = current(src, &out.files, path) else {
                lost.push(file_level());
                continue;
            };
            let (base, new) = match op {
                Op::Write { base, content, kind, .. } => (*base, Some((*kind, *content))),
                Op::Delete { base, .. } => (Some(*base), None),
            };
            let result = if cur.map(|c| c.1) == base {
                crate::metrics::FAST_APPLIES.add(1);
                new // Tier 0: the note was written against exactly what head has.
            } else {
                let mut get = |h: Option<Hash>| -> Result<Option<Vec<u8>>> {
                    let Some(h) = h else { return Ok(None) };
                    if let Some(b) = out.blobs.get(&h).or(cache.get(&h)) {
                        return Ok(Some(b.clone()));
                    }
                    let b = src.blob(h)?;
                    cache.insert(h, b.clone());
                    Ok(Some(b))
                };
                let (Ok(b), Ok(o), Ok(t)) = (get(base), get(cur.map(|c| c.1)), get(new.map(|n| n.1))) else {
                    lost.push(file_level());
                    continue;
                };
                let too_big = [&b, &o, &t].iter().any(|v| v.as_ref().is_some_and(|v| v.len() > MAX_MERGE));
                if too_big {
                    lost.push(file_level());
                    continue;
                }
                match merge3(path, b.as_deref(), o.as_deref(), t.as_deref()) {
                    Ok(None) => None,
                    Ok(Some(bytes)) => {
                        let h = Hash::of(&bytes);
                        let kind = new.or(cur).map_or(Kind::File, |n| n.0);
                        if Some(h) != new.map(|n| n.1) && Some(h) != cur.map(|c| c.1) {
                            out.blobs.insert(h, bytes);
                        }
                        Some((kind, h))
                    }
                    Err(atoms) => {
                        lost.extend(atoms.into_iter().map(|a| (path.to_owned(), a)));
                        continue;
                    }
                }
            };
            if result.is_some() && cur.is_none() && !fits(src, &out.files, &staged, path).unwrap_or(false) {
                lost.push(file_level());
                continue;
            }
            staged.insert(path.to_owned(), result);
        }
        if lost.is_empty() {
            for (path, state) in staged {
                last_writer.insert(path.clone(), *id);
                out.files.insert(path, state);
            }
            out.applied.push(*id);
        } else {
            for (path, atom) in lost {
                out.conflicts.push(Conflict { atom, kept: last_writer.get(&path).copied(), losing: *id });
            }
        }
    }
    // Merged blobs that a later note replaced are not referenced by head; drop them.
    let live: HashSet<Hash> = out.files.values().flatten().map(|f| f.1).collect();
    out.blobs.retain(|h, _| live.contains(h));
    Ok(out)
}

/// What a pass changed for notices (SPEC §8). "New" is relative to the previous pass, so a note
/// whose outcome flips under a refold is reported again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A note newly in head, with the atoms it changed.
    Applied { note: Hash, session: u64, agent: String, changes: Vec<AtomChange> },
    /// A note newly out of head because of `conflict`. The texts are the atom's.
    Conflicted { note: Hash, session: u64, conflict: Conflict, base: Option<String>, kept: Option<String>, theirs: Option<String> },
}

/// One atom a note changed, with its signature before and after (`None` = absent; the
/// file-level atom has no signature, so it is `Some("")` when present).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtomChange {
    pub atom: AtomKey,
    pub before: Option<String>,
    pub after: Option<String>,
    /// Identifiers the new text mentions (empty if deleted), for dependents (SPEC §8.1 rule 3).
    pub idents: Vec<String>,
}

/// The atoms that differ between two versions of a file (`None` = absent).
pub fn diff_atoms(path: &str, old: Option<&[u8]>, new: Option<&[u8]>) -> Vec<AtomChange> {
    let details = |b: Option<&[u8]>| b.map(|b| atom_details(path, b)).unwrap_or_default();
    let (old, new) = (details(old), details(new));
    let sig = |d: &AtomDetail| d.sig.clone().unwrap_or_default();
    let mut out = vec![];
    for key in old.keys().chain(new.keys().filter(|k| !old.contains_key(*k))) {
        let (o, n) = (old.get(key), new.get(key));
        if o.map(|d| d.hash) != n.map(|d| d.hash) {
            let idents = n.map(|d| crate::note::idents(&d.text).into_iter().map(str::to_owned).collect()).unwrap_or_default();
            out.push(AtomChange { atom: key.clone(), before: o.map(sig), after: n.map(sig), idents });
        }
    }
    out
}

/// The atoms `note` changed, from each op's base to its content.
fn changes_of(st: &State, note: &Note) -> Result<Vec<AtomChange>> {
    let blob = |id: Option<Hash>| id.map(|id| st.blob(id)).transpose();
    let mut out = vec![];
    for op in &note.ops {
        let (base, new) = op.sides();
        out.extend(diff_atoms(op.path(), blob(base)?.as_deref(), blob(new)?.as_deref()));
    }
    Ok(out)
}

/// The text of `atom` in the file `id`, if both exist.
fn atom_text(st: &State, id: Option<Hash>, atom: &AtomKey) -> Result<Option<String>> {
    let Some(id) = id else { return Ok(None) };
    let Some(bytes) = st.get_blob(id)? else { return Ok(None) };
    Ok(atom_details(&atom.path, &bytes).remove(atom).map(|d| d.text))
}

/// Notes reported by the previous pass, to report only what changed.
#[derive(Default)]
struct Reported {
    applied: HashSet<Hash>,
    conflicted: HashSet<Hash>,
}

fn pass_events(
    st: &State,
    head: Hash,
    notes: &[(Hash, Note)],
    folded: &Folded,
    seen: &mut Reported,
) -> Result<Vec<Event>> {
    let by_id: HashMap<Hash, &Note> = notes.iter().map(|(id, n)| (*id, n)).collect();
    let mut out = vec![];
    for id in folded.applied.iter().filter(|id| !seen.applied.contains(*id)) {
        let n = by_id[id];
        out.push(Event::Applied { note: *id, session: n.session, agent: n.author.clone(), changes: changes_of(st, n)? });
    }
    for c in folded.conflicts.iter().filter(|c| !seen.conflicted.contains(&c.losing)) {
        let n = by_id[&c.losing];
        let (base, theirs) = n.ops.iter().find(|op| op.path() == c.atom.path).map_or((None, None), Op::sides);
        let kept = st.lookup(head, &c.atom.path)?.map(|(_, id)| id);
        out.push(Event::Conflicted {
            note: c.losing,
            session: n.session,
            conflict: c.clone(),
            base: atom_text(st, base, &c.atom)?,
            kept: atom_text(st, kept, &c.atom)?,
            theirs: atom_text(st, theirs, &c.atom)?,
        });
    }
    seen.applied = folded.applied.iter().copied().collect();
    seen.conflicted = folded.conflicts.iter().map(|c| c.losing).collect();
    Ok(out)
}

/// The board's refs as the daemon last saw them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Heads {
    pub head: Hash,
    pub promoted: Hash,
    pub conflicts: u64,
    /// The highest `gseq` the latest pass folded; the log may already hold more (combiner lag).
    pub folded: u64,
    pub pass_ms: u64,
    /// Files in head that fail the gate, holding up promotion (SPEC §9).
    pub blocked: Vec<String>,
    /// Gate checks that failed on the latest promotion candidate (SPEC §9.2).
    pub failing: Vec<String>,
    /// Passes so far, so a caller can wait for one that started after its request.
    pub passes: u64,
    /// Every note up to this `gseq` is decided (the promoted mark).
    pub promoted_gseq: u64,
    /// When the promoted root last changed, in milliseconds since the Unix epoch (0: not yet).
    pub promoted_ms: u64,
}

/// The gate's checks beyond parsing (SPEC §9.2), answered without blocking the combiner.
pub trait Gate: Send + Sync {
    /// Whether `root` (the head after folding every note up to `gseq`) may be promoted.
    /// `Pending` means the checks are running; a later [`Gate::passed`] reports the outcome.
    fn check(&self, root: Hash, gseq: u64) -> GateState;
    /// A candidate whose checks passed since the last call, to promote even if head moved on.
    fn passed(&self) -> Option<(Hash, u64)>;
    /// Notes found to break the gate (by bisection) since the last call: rejected for good.
    fn rejected(&self) -> Vec<Hash>;
    /// `root` was just promoted (deploy hooks, the backstop run).
    fn promoted(&self, root: Hash);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateState {
    Pass,
    Pending,
    /// The names of the failing checks.
    Fail(Vec<String>),
}

/// Handle to the combiner thread, which owns the only write connection to `state.db`.
pub struct Combiner {
    wake: SyncSender<()>,
    promote: Arc<AtomicBool>,
}

impl Combiner {
    /// Starts the thread and runs a first pass over whatever is already in the log.
    ///
    /// Head moves on every pass. Promotion happens at most once per `promote_every` (`None`:
    /// only on [`Combiner::promote_now`]), which gives rank order a window to decide between
    /// notes that arrive close together (SPEC §4.5: σ priority holds until promotion).
    /// Each pass's [`Event`]s go to `events`, if given.
    pub fn spawn(
        repo: &Path,
        heads: tokio::sync::watch::Sender<Heads>,
        promote_every: Option<Duration>,
        events: Option<Sender<Vec<Event>>>,
        gate: Option<Arc<dyn Gate>>,
    ) -> Result<Self> {
        let mut st = State::open_board(repo)?;
        let mut rejected = st.rejected_notes()?;
        let (wake, rx) = sync_channel::<()>(1);
        let promote = Arc::new(AtomicBool::new(false));
        let forced = promote.clone();
        std::thread::Builder::new()
            .name("kintrel-combiner".into())
            .spawn(move || {
                let tick = promote_every.unwrap_or(Duration::from_secs(3600));
                let mut last = Instant::now();
                let mut seen = Reported::default();
                loop {
                    if let Err(RecvTimeoutError::Disconnected) = rx.recv_timeout(tick) {
                        return;
                    }
                    let due = promote_every.is_some_and(|d| last.elapsed() >= d);
                    let span = tracing::debug_span!("pass").entered();
                    let started = Instant::now();
                    match pass(&mut st, &heads, forced.swap(false, Ordering::AcqRel) || due, gate.as_deref(), &mut rejected) {
                        Ok(out) => {
                            tracing::debug!(notes = out.notes.len(), promoted = out.promoted, ms = started.elapsed().as_millis() as u64, "pass done");
                            if out.promoted {
                                last = Instant::now();
                            }
                            // A requested promotion waiting on gate checks stays requested, so
                            // the pass the CI worker wakes asks the gate again.
                            if out.gate_pending {
                                forced.store(true, Ordering::Release);
                            }
                            if let Some(tx) = &events {
                                match pass_events(&st, out.head, &out.notes, &out.folded, &mut seen) {
                                    Ok(ev) if ev.is_empty() => {}
                                    Ok(ev) => {
                                        let _ = tx.send(ev);
                                    }
                                    Err(e) => tracing::error!("notices failed: {e}"),
                                }
                            }
                        }
                        Err(e) => tracing::error!("pass failed: {e}"),
                    }
                    drop(span);
                }
            })
            .expect("spawn combiner thread");
        let c = Combiner { wake, promote };
        c.wake();
        Ok(c)
    }

    /// A handle that asks for a pass, for other threads (the CI worker).
    pub fn waker(&self) -> impl Fn(()) + Send + Sync + 'static {
        let wake = self.wake.clone();
        move |()| {
            let _ = wake.try_send(());
        }
    }

    /// Runs a pass that promotes head if the gate passes.
    pub fn promote_now(&self) {
        self.promote.store(true, Ordering::Release);
        self.wake();
    }

    /// Asks for a pass. Wakes that arrive during a pass collapse into one more pass, which
    /// reads the log afresh, so a committed note is never missed.
    pub fn wake(&self) {
        let _ = self.wake.try_send(());
    }
}

/// What one pass did.
struct PassOut {
    promoted: bool,
    gate_pending: bool,
    head: Hash,
    notes: Vec<(Hash, Note)>,
    folded: Folded,
}

/// One combiner pass.
fn pass(
    st: &mut State,
    heads: &tokio::sync::watch::Sender<Heads>,
    may_promote: bool,
    gate: Option<&dyn Gate>,
    rejected: &mut HashSet<Hash>,
) -> Result<PassOut> {
    let start = std::time::Instant::now();
    for note in gate.map(Gate::rejected).unwrap_or_default() {
        st.reject(note)?;
        rejected.insert(note);
    }
    let mut promoted = st.get_ref(state::PROMOTED)?.ok_or_else(|| Error::Corrupt("no promoted ref".into()))?;
    let mut mark = st.get_mark(state::PROMOTED_GSEQ)?;
    // A candidate whose checks finished passing while head moved on is promoted first.
    if let Some((root, gseq)) = gate.and_then(Gate::passed)
        && gseq > mark
    {
        let head = st.get_ref(state::HEAD)?.ok_or_else(|| Error::Corrupt("no head ref".into()))?;
        st.promote_to(head, root, gseq)?;
        crate::metrics::PROMOTIONS.add(1);
        (promoted, mark) = (root, gseq);
        gate.expect("a gate passed it").promoted(root);
    }
    let rows = st.log_notes_after(mark)?;
    let high = rows.last().map_or(mark, |r| r.0);
    let notes: Vec<(Hash, Note)> = rows.into_iter().filter(|(_, id, _)| !rejected.contains(id)).map(|(_, id, n)| (id, n)).collect();
    let folded = fold(&Promoted { st, root: promoted }, &notes)?;
    let blocked = parse_gate(st, &folded)?;
    let working: Vec<Hash> = notes.iter().map(|(id, _)| *id).collect();
    let head = st.batch(|s| {
        for bytes in folded.blobs.values() {
            s.put_blob(bytes)?;
        }
        let head = s.apply(promoted, &folded.files)?;
        s.record_pass(head, &working, &folded.applied, &folded.conflicts)?;
        // A Resolution note in head resolves the conflict it names, if it writes that file.
        let applied: HashSet<&Hash> = folded.applied.iter().collect();
        let resolved: Vec<(Hash, Vec<&str>)> = notes
            .iter()
            .filter(|(id, _)| applied.contains(id))
            .filter_map(|(_, n)| match n.kind {
                crate::note::NoteKind::Resolution { conflict } => Some((conflict, n.ops.iter().map(Op::path).collect())),
                crate::note::NoteKind::Code => None,
            })
            .collect();
        s.resolve_conflicts(&resolved)?;
        Ok(head)
    })?;
    // The checks gate runs once head is committed, so the CI side can read it.
    // ponytail: promote the whole pass or nothing; partitioned promotion (SPEC §9.1) is not built,
    // since rejection already keeps one bad note from holding everyone back.
    let mut failing = vec![];
    let mut gate_pending = false;
    let promote = may_promote
        && blocked.is_empty()
        && !notes.is_empty()
        && match gate.map_or(GateState::Pass, |g| g.check(head, high)) {
            GateState::Pass => true,
            GateState::Pending => {
                gate_pending = true;
                false
            }
            GateState::Fail(checks) => {
                failing = checks;
                false
            }
        };
    if promote {
        st.promote_to(head, head, high)?;
        crate::metrics::PROMOTIONS.add(1);
        (promoted, mark) = (head, high);
        if let Some(g) = gate {
            g.promoted(head);
        }
    }
    let before = heads.borrow().clone();
    let promoted_ms = if promoted != before.promoted { crate::now_ms() } else { before.promoted_ms };
    let passes = before.passes + 1;
    crate::metrics::COMBINER_PASS_MS.add(start.elapsed().as_millis() as u64);
    heads.send_replace(Heads {
        head,
        promoted,
        conflicts: st.open_conflict_count()?,
        folded: high,
        pass_ms: start.elapsed().as_millis() as u64,
        blocked,
        failing,
        passes,
        promoted_gseq: mark,
        promoted_ms,
    });
    Ok(PassOut { promoted: promote, gate_pending, head, notes, folded })
}

/// The Phase 1 gate (SPEC §9.2): every changed file in a supported language parses. A failure
/// holds promotion back until a later note fixes the file; nothing is rejected yet, because
/// agents post after every edit and a half-finished edit is normal.
// (Failing gate checks, by contrast, are rejected by bisection in the CI worker.)
fn parse_gate(st: &State, folded: &Folded) -> Result<Vec<String>> {
    let mut blocked = vec![];
    for (path, state) in &folded.files {
        let Some((kind, id)) = state else { continue };
        if !matches!(kind, Kind::File | Kind::Exec) || kintrel_lang::lang_for_path(path).is_none() {
            continue;
        }
        let bytes = match folded.blobs.get(id) {
            Some(b) => b.clone(),
            None => st.blob(*id)?,
        };
        if kintrel_lang::segment_checked(path, &bytes).is_none() {
            blocked.push(path.clone());
        }
    }
    Ok(blocked)
}

/// A fold [`Source`] backed by the store, folding onto `root`.
pub struct Promoted<'a> {
    pub st: &'a State,
    pub root: Hash,
}

impl Source for Promoted<'_> {
    fn promoted(&self, path: &str) -> Result<Option<(Kind, Hash)>> {
        self.st.lookup(self.root, path)
    }
    fn blob(&self, id: Hash) -> Result<Vec<u8>> {
        self.st.blob(id)
    }
}

/// The file at `path` in head so far.
fn current(src: &dyn Source, files: &BTreeMap<String, FileState>, path: &str) -> Result<FileState> {
    match files.get(path) {
        Some(state) => Ok(*state),
        None => Ok(src.promoted(path)?.filter(|(k, _)| *k != Kind::Dir)),
    }
}

/// A new file at `path` keeps head a tree: no parent is a file, and the path is not a directory.
fn fits(
    src: &dyn Source,
    files: &BTreeMap<String, FileState>,
    staged: &BTreeMap<String, FileState>,
    path: &str,
) -> Result<bool> {
    let is_file = |p: &str| -> Result<bool> {
        Ok(match staged.get(p).or(files.get(p)) {
            Some(state) => state.is_some(),
            None => src.promoted(p)?.is_some_and(|(k, _)| k != Kind::Dir),
        })
    };
    let mut at = 0;
    while let Some(i) = path[at..].find('/') {
        if is_file(&path[..at + i])? {
            return Ok(false);
        }
        at += i + 1;
    }
    // A promoted directory counts even if every file under it was deleted: strict and simple.
    let under = format!("{path}/");
    let child_written = staged.iter().chain(files.iter()).any(|(p, s)| s.is_some() && p.starts_with(&under));
    Ok(!child_written && !src.promoted(path)?.is_some_and(|(k, _)| k == Kind::Dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note::NoteKind;

    struct Mem {
        files: BTreeMap<String, Vec<u8>>,
        blobs: HashMap<Hash, Vec<u8>>,
    }

    impl Mem {
        fn new(files: &[(&str, &str)]) -> Self {
            let mut m = Mem { files: BTreeMap::new(), blobs: HashMap::new() };
            for (p, c) in files {
                m.files.insert((*p).into(), c.as_bytes().to_vec());
                m.put(c);
            }
            m
        }
        fn put(&mut self, c: &str) -> Hash {
            let h = Hash::of(c.as_bytes());
            self.blobs.insert(h, c.as_bytes().to_vec());
            h
        }
    }

    impl Source for Mem {
        fn promoted(&self, path: &str) -> Result<Option<(Kind, Hash)>> {
            if let Some(b) = self.files.get(path) {
                return Ok(Some((Kind::File, Hash::of(b))));
            }
            let dir = format!("{path}/");
            Ok(self.files.keys().any(|p| p.starts_with(&dir)).then_some((Kind::Dir, Hash::default())))
        }
        fn blob(&self, id: Hash) -> Result<Vec<u8>> {
            self.blobs.get(&id).cloned().ok_or_else(|| Error::Corrupt(format!("blob {id}")))
        }
    }

    fn note(session: u64, seq: u64, parents: Vec<Hash>, ops: Vec<Op>) -> (Hash, Note) {
        let n = Note {
            author: format!("s{session}"),
            session,
            rank: session,
            seq,
            base_root: Hash::default(),
            parents,
            kind: NoteKind::Code,
            ops,
            reads: vec![],
        };
        (n.id(), n)
    }

    fn write(m: &mut Mem, path: &str, base: Option<&str>, new: &str) -> Op {
        Op::Write { path: path.into(), base: base.map(|b| Hash::of(b.as_bytes())), content: m.put(new), kind: Kind::File }
    }

    fn head(m: &Mem, f: &Folded, path: &str) -> Option<String> {
        let state = f.files.get(path).copied().unwrap_or_else(|| m.promoted(path).unwrap());
        state.map(|(_, h)| String::from_utf8(f.blobs.get(&h).cloned().unwrap_or_else(|| m.blob(h).unwrap())).unwrap())
    }

    const A: &str = "def f():\n    return 1\n\n\ndef g():\n    return 2\n";

    #[test]
    fn disjoint_edits_from_two_sessions_both_apply() {
        let mut m = Mem::new(&[("a.py", A)]);
        let n1 = note(1, 1, vec![], vec![write(&mut m, "a.py", Some(A), &A.replace("return 1", "return 10"))]);
        let n2 = note(2, 1, vec![], vec![write(&mut m, "a.py", Some(A), &A.replace("return 2", "return 20"))]);
        let f = fold(&m, &[n2.clone(), n1.clone()]).unwrap();
        assert_eq!(f.applied, [n1.0, n2.0]);
        assert_eq!(head(&m, &f, "a.py").unwrap(), A.replace("return 1", "return 10").replace("return 2", "return 20"));
    }

    #[test]
    fn lower_rank_wins_and_the_loser_is_atomic() {
        let mut m = Mem::new(&[("a.py", A), ("b.py", "x = 1\n")]);
        let n1 = note(1, 1, vec![], vec![write(&mut m, "a.py", Some(A), &A.replace("return 1", "return 10"))]);
        let n2 = note(2, 1, vec![], vec![
            write(&mut m, "b.py", Some("x = 1\n"), "x = 2\n"),
            write(&mut m, "a.py", Some(A), &A.replace("return 1", "return 11")),
        ]);
        let f = fold(&m, &[n2.clone(), n1.clone()]).unwrap();
        assert_eq!(f.applied, [n1.0]);
        assert_eq!(f.conflicts, [Conflict {
            atom: "a.py::def f".parse().unwrap(),
            kept: Some(n1.0),
            losing: n2.0,
        }]);
        assert_eq!(head(&m, &f, "b.py").unwrap(), "x = 1\n", "the losing note's other file is untouched");
    }

    #[test]
    fn causal_order_beats_rank() {
        // Session 1 (lower rank) built on session 2's note, so it must fold after it.
        let mut m = Mem::new(&[("a.py", A)]);
        let v2 = A.replace("return 1", "return 10");
        let n2 = note(2, 1, vec![], vec![write(&mut m, "a.py", Some(A), &v2)]);
        let v1 = v2.replace("return 10", "return 100");
        let n1 = note(1, 1, vec![n2.0], vec![write(&mut m, "a.py", Some(&v2), &v1)]);
        let f = fold(&m, &[n1.clone(), n2.clone()]).unwrap();
        assert_eq!(f.applied, [n2.0, n1.0]);
        assert_eq!(head(&m, &f, "a.py").unwrap(), v1);
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        #[derive(Debug, Clone)]
        enum Edit {
            Body(usize),
            Delete(usize),
            Add,
        }

        const FNS: usize = 6;

        fn render(fns: &[(String, String)]) -> String {
            fns.iter().map(|(n, b)| format!("def {n}():\n    return {b}\n")).collect::<Vec<_>>().join("\n\n")
        }

        fn base() -> Vec<(String, String)> {
            (0..FNS).map(|i| (format!("f{i}"), i.to_string())).collect()
        }

        /// One agent's edits on the base file. Values and new names are unique per agent.
        fn edited(agent: usize, edits: &[Edit]) -> Vec<(String, String)> {
            let mut fns = base();
            for (k, e) in edits.iter().enumerate() {
                match e {
                    Edit::Body(i) => {
                        if let Some(f) = fns.iter_mut().find(|f| f.0 == format!("f{i}")) {
                            f.1 = format!("{}", 1000 * (agent + 1) + i);
                        }
                    }
                    Edit::Delete(i) => fns.retain(|f| f.0 != format!("f{i}")),
                    Edit::Add => {
                        let at = k % (fns.len() + 1);
                        fns.insert(at, (format!("new{agent}_{k}"), "0".into()));
                    }
                }
            }
            fns
        }

        fn edit() -> impl Strategy<Value = Edit> {
            prop_oneof![(0..FNS).prop_map(Edit::Body), (0..FNS).prop_map(Edit::Delete), Just(Edit::Add)]
        }

        proptest! {
            #[test]
            fn fold_is_deterministic_atomic_and_parses(
                agents in prop::collection::vec(prop::collection::vec(edit(), 1..4), 2..6),
                shuffle in Just((0..6usize).collect::<Vec<_>>()).prop_shuffle(),
            ) {
                let base_text = render(&base());
                let mut m = Mem::new(&[("a.py", &base_text)]);
                let mut notes = Vec::new();
                for (a, edits) in agents.iter().enumerate() {
                    let ops = vec![
                        write(&mut m, &format!("own{a}.txt"), None, "mine\n"),
                        write(&mut m, "a.py", Some(&base_text), &render(&edited(a, edits))),
                    ];
                    notes.push(note(a as u64 + 1, 1, vec![], ops));
                }
                let f = fold(&m, &notes).unwrap();

                // Determinism: any arrival order gives the same result.
                let shuffled: Vec<_> = shuffle.iter().filter(|&&i| i < notes.len()).map(|&i| notes[i].clone()).collect();
                prop_assert_eq!(&fold(&m, &shuffled).unwrap(), &f);

                // Head parses: merging never breaks parsing.
                let head_text = head(&m, &f, "a.py").unwrap();
                prop_assert!(kintrel_lang::segment_checked("a.py", head_text.as_bytes()).is_some(), "{}", head_text);

                for (a, edits) in agents.iter().enumerate() {
                    let applied = f.applied.contains(&notes[a].0);
                    // Atomicity: the marker file is in head exactly when the note applied.
                    prop_assert_eq!(head(&m, &f, &format!("own{a}.txt")).is_some(), applied);
                    // An applied note's own changes all survive; a conflicted note's leave no trace.
                    let mine = edited(a, edits);
                    for (k, e) in edits.iter().enumerate() {
                        let present = match e {
                            Edit::Body(i) => mine.iter().any(|f| f.0 == format!("f{i}"))
                                .then(|| head_text.contains(&format!("return {}", 1000 * (a + 1) + i))),
                            Edit::Add => Some(head_text.contains(&format!("def new{a}_{k}()"))),
                            Edit::Delete(_) => None,
                        };
                        if let Some(present) = present {
                            prop_assert_eq!(present, applied, "agent {} edit {:?}\n{}", a, e, head_text);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_broken_note_conflicts_instead_of_failing_the_pass() {
        let mut m = Mem::new(&[("a.py", A)]);
        let ok = note(1, 1, vec![], vec![write(&mut m, "a.py", Some(A), &A.replace("return 1", "return 10"))]);
        // Content blob that the store does not have, against a base that forces a merge.
        let missing = Op::Write { path: "a.py".into(), base: Some(Hash::of(A.as_bytes())), content: Hash([7; 32]), kind: Kind::File };
        let broken = note(2, 1, vec![], vec![missing]);
        let f = fold(&m, &[ok.clone(), broken.clone()]).unwrap();
        assert_eq!(f.applied, [ok.0]);
        assert_eq!(f.conflicts.len(), 1);
        assert_eq!(f.conflicts[0].losing, broken.0);
    }

    #[test]
    fn file_and_directory_cannot_collide() {
        let mut m = Mem::new(&[("a.py", A)]);
        let n1 = note(1, 1, vec![], vec![write(&mut m, "x", None, "file\n")]);
        let n2 = note(2, 1, vec![], vec![write(&mut m, "x/y", None, "nested\n")]);
        let n3 = note(3, 1, vec![], vec![write(&mut m, "a.py/z", None, "under a file\n")]);
        let f = fold(&m, &[n1.clone(), n2, n3]).unwrap();
        assert_eq!(f.applied, [n1.0]);
        assert_eq!(f.conflicts.len(), 2);
    }
}
