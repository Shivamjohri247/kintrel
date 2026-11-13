//! `kintreld`: one daemon per board, serving JSON-RPC on `.kintrel/kintrel.sock` (SPEC §3, §12.1).
//!
//! Sessions live in memory for now: a daemon restart ends them (their notes stay in the log).
//! The combiner thread folds notes into head after each post (SPEC §7).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use kintrel_core::combine::{Combiner, Heads, diff_atoms};
use kintrel_core::note::{AtomKey, AtomRead, Note, NoteKind, Op, atom_hashes};
use kintrel_core::state::{self, State};
use kintrel_core::tree::Kind;
use kintrel_core::{Hash, board, encode};
use kintrel_core::log::{AppendError, Durability, LogWriter, NewBlob, NewNote};
use kintrel_core::path::is_safe_rel_path;
use kintrel_core::proto::{self, MAX_BLOB, MAX_OPS, MAX_SUMMARY, Request, Response, RpcError, code};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::notices::{self, Coord};
use crate::rpc::{read_frame, write_frame};

mod checks;
mod revert;
mod sync;
mod tier3;

pub struct Options {
    pub repo: PathBuf,
    pub durability: Durability,
    /// How often head is promoted when the gate passes; `None` = only on `promote.now`.
    pub promote_every: Option<std::time::Duration>,
}

/// The board's person key, created on first start.
fn person_key(repo: &Path) -> io::Result<String> {
    let path = board::person_key(repo).map_err(io::Error::other)?;
    let keys = path.parent().expect("in the keys directory");
    fs::create_dir_all(keys)?;
    kintrel_core::os::set_private_dir(keys)?;
    match fs::read_to_string(&path) {
        Ok(key) => Ok(key),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let key = format!("{}{}", crate::ipc::random_hex(), crate::ipc::random_hex());
            // The keys directory is private; `create_new` refuses a planted file.
            let mut open = fs::OpenOptions::new();
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
            let mut f = open.write(true).create_new(true).open(&path)?;
            io::Write::write_all(&mut f, key.as_bytes())?;
            Ok(key)
        }
        Err(e) => Err(e),
    }
}

pub(crate) const TIER3_AGENT: &str = "kintrel-tier3";
/// Agent names only the daemon uses: notes under them are Kintrel's own (sync, Tier 3).
const RESERVED_AGENTS: &[&str] = &[crate::git::SYNC_AGENT, TIER3_AGENT];

pub fn socket_path(repo: &Path) -> io::Result<PathBuf> {
    let dir = board::runtime_dir(repo).map_err(io::Error::other)?;
    Ok(crate::ipc::endpoint(&dir))
}

/// Runs the daemon until the process exits.
pub async fn run(opts: Options) -> io::Result<()> {
    let not_board = || io::Error::new(io::ErrorKind::NotFound, format!("{} is not a kintrel board (run `kintrel init`)", opts.repo.display()));
    if !board::state_db(&opts.repo).exists() {
        return Err(not_board());
    }
    let run_dir = board::runtime_dir(&opts.repo).map_err(io::Error::other)?;
    fs::create_dir_all(&run_dir)?;
    kintrel_core::os::set_private_dir(&run_dir)?;
    let mut listener = crate::ipc::Listener::bind(&run_dir).await?;
    let person_key = person_key(&opts.repo)?;
    let log = LogWriter::open(&board::log_db(&opts.repo), opts.durability).map_err(io::Error::other)?;
    let reader = open_reader(&opts.repo).map_err(io::Error::other)?;
    // Open the pool up front: opening a connection mid-request costs milliseconds.
    let mut readers = vec![];
    for _ in 1..std::thread::available_parallelism().map_or(4, std::num::NonZero::get) {
        readers.push(open_reader(&opts.repo).map_err(io::Error::other)?);
    }
    let head = reader.get_ref(state::HEAD).map_err(io::Error::other)?.ok_or_else(not_board)?;
    let promoted = reader.get_ref(state::PROMOTED).map_err(io::Error::other)?.ok_or_else(not_board)?;
    let first_session = reader.max_session().map_err(io::Error::other)? + 1;
    readers.push(reader);

    let (publish, heads) = tokio::sync::watch::channel(Heads { head, promoted, ..Heads::default() });
    let (events, inbound) = std::sync::mpsc::channel();
    let ci = Arc::new(kintrel_ci::Ci::open(&opts.repo).map_err(io::Error::other)?);
    let worker = crate::ci_worker::CiWorker::start(opts.repo.clone(), ci.clone())?;
    let gate: Arc<dyn kintrel_core::combine::Gate> = worker.clone();
    let combiner = Combiner::spawn(&opts.repo, publish, opts.promote_every, Some(events), Some(gate)).map_err(io::Error::other)?;
    let _ = worker.wake_combiner.set(Box::new(combiner.waker()));
    let board = Arc::new(Board {
        repo: opts.repo,
        readers: Mutex::new(readers),
        heads,
        combiner,
        log,
        sessions: Mutex::new(HashMap::new()),
        coord: Mutex::new(Coord::default()),
        ci,
        worker: worker.clone(),
        next_session: AtomicU64::new(first_session),
        started: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos(),
        person_key,
    });
    let reporter = board.clone();
    let _ = worker.report.set(Box::new(move |failed| reporter.coord.lock().unwrap().ci_failed(&failed)));
    let r = board.clone();
    let _ = worker.report_rejected.set(Box::new(move |(session, note, failing)| r.coord.lock().unwrap().rejected(session, note, &failing)));
    let r = board.clone();
    let _ = worker.report_incident.set(Box::new(move |failing| r.coord.lock().unwrap().incident(&failing)));
    // Tier 3: new conflicts go to a task that may propose a merge (when kintrel.toml enables it).
    let (conflicts, mut conflicted) = tokio::sync::mpsc::unbounded_channel::<(u64, kintrel_core::combine::Conflict)>();
    let proposer = board.clone();
    tokio::spawn(async move {
        let mut tried = HashSet::new();
        let mut token = None;
        while let Some((session, conflict)) = conflicted.recv().await {
            if tried.insert(conflict.id())
                && let Err(e) = proposer.tier3(session, &conflict, &mut token).await
            {
                tracing::warn!("tier3: {}: {}", conflict.atom, e.message);
            }
        }
    });
    let router = board.clone();
    std::thread::Builder::new().name("kintrel-notices".into()).spawn(move || {
        for events in inbound {
            let applied = events.iter().any(|e| matches!(e, kintrel_core::combine::Event::Applied { .. }));
            for e in &events {
                if let kintrel_core::combine::Event::Conflicted { session, conflict, .. } = e {
                    let _ = conflicts.send((*session, conflict.clone()));
                }
            }
            router.coord.lock().unwrap().route(events);
            if applied {
                worker.feedback(router.heads.borrow().head);
            }
        }
    })?;
    loop {
        let stream = listener.accept().await?;
        let key = listener.key.clone();
        tokio::spawn(serve(board.clone(), stream, key));
    }
}

async fn serve(board: Arc<Board>, mut stream: crate::ipc::ServerStream, key: String) {
    if crate::ipc::answer(&mut stream, &key).await.is_err() {
        return;
    }
    while let Ok(Some(frame)) = read_frame(&mut stream).await {
        let (id, outcome) = match serde_json::from_slice::<Request>(&frame) {
            Ok(req) => (req.id, board.dispatch(&req.method, req.params).await),
            Err(e) => (0, Err(RpcError::new(code::PARSE_ERROR, e.to_string()))),
        };
        let (result, error) = match outcome {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e)),
        };
        let resp = Response { jsonrpc: "2.0".into(), id, result, error };
        let body = serde_json::to_vec(&resp).expect("responses always serialize");
        if write_frame(&mut stream, &body).await.is_err() {
            return;
        }
    }
}

struct Session {
    id: u64,
    agent: String,
    rank: u64,
    seq: u64,
    base_root: Hash,
    last_note: Option<Hash>,
    /// Notes the view's base was built on; parents of the next note (SPEC §7.2).
    base_parents: Vec<Hash>,
    /// `gseq` of the last note, and every note since the last refresh.
    last_gseq: u64,
    since_refresh: Vec<Hash>,
    /// The session's own writes on top of `base_root`: path → new file, or `None` if deleted.
    overlay: HashMap<String, Option<(Kind, Hash)>>,
    /// Atoms read since the previous note (SPEC §8.2).
    reads: BTreeMap<AtomKey, Hash>,
    /// The current one-second window and the posts in it (`MAX_POSTS_PER_SEC`).
    posts: (std::time::Instant, u32),
}

struct Board {
    repo: PathBuf,
    /// Idle read connections (state.db with log.db attached); `with_reader` opens more on demand.
    readers: Mutex<Vec<State>>,
    /// Published by the combiner after each pass.
    heads: tokio::sync::watch::Receiver<Heads>,
    combiner: Combiner,
    log: LogWriter,
    // A per-session async lock serializes one session's reads and posts, keeping `seq` gapless.
    sessions: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Session>>>>,
    /// Footprints, inboxes, claims and intents (SPEC §7.4, §8).
    coord: Mutex<Coord>,
    ci: Arc<kintrel_ci::Ci>,
    worker: Arc<crate::ci_worker::CiWorker>,
    next_session: AtomicU64,
    started: u128,
    /// `board::person_key`'s contents.
    person_key: String,
}

/// A file as the session sees it.
struct ViewFile {
    kind: Kind,
    id: Hash,
    bytes: Vec<u8>,
}

/// What `post` learned about one file, computed off the async runtime.
struct Prepared {
    path: String,
    current: Option<ViewFile>,
    content: Option<Vec<u8>>,
    base: Option<Hash>,
    /// Set if a parent path is a file, or the path itself is a directory.
    blocked: Option<String>,
    atoms: Vec<AtomKey>,
    /// The changed atoms with the identifiers their new text mentions (for dependents).
    changes: Vec<kintrel_core::combine::AtomChange>,
}

impl Board {
    /// Runs one request, in a span named after its method (`KINTREL_LOG=kintrel=debug`).
    async fn dispatch(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        use tracing::Instrument;
        let started = std::time::Instant::now();
        let r = self.dispatch_method(method, params).instrument(tracing::debug_span!("rpc", method)).await;
        match &r {
            Ok(_) => tracing::debug!(method, us = started.elapsed().as_micros() as u64, "ok"),
            Err(e) => tracing::info!(method, code = e.code, "failed: {}", e.message),
        }
        r
    }

    async fn dispatch_method(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            proto::SESSION_START => reply(self.session_start(parse(params)?).await),
            proto::SESSION_REFRESH => reply(self.refresh(parse(params)?).await),
            proto::POST => reply(self.post(parse(params)?).await),
            proto::READ_FILE => reply(self.read_file(parse(params)?).await),
            proto::STATUS => reply(Ok(self.status())),
            proto::PROMOTE_NOW => reply(self.promote_now(parse(params)?).await),
            proto::INBOX => reply(self.inbox(parse(params)?).await),
            proto::INTENT => reply(self.announce(parse(params)?, true).await),
            proto::CLAIM => reply(self.announce(parse(params)?, false).await),
            proto::RELEASE => reply(self.release(parse(params)?).await),
            proto::SESSION_HEARTBEAT => reply(self.heartbeat(parse(params)?).await),
            proto::SESSION_END => reply(self.end(parse(params)?).await),
            proto::REVERT => reply(self.revert(parse(params)?).await),
            proto::CHECKS_RUN => reply(self.checks(parse(params)?).await),
            proto::SYNC => reply(self.sync(parse(params)?).await),
            proto::METRICS => reply(Ok(self.metrics())),
            _ => Err(RpcError::new(code::METHOD_NOT_FOUND, format!("unknown method {method:?}"))),
        }
    }

    /// A client's session. Agent names end up in Git commit messages (export trailers), so they
    /// are one safe word; `git` and `kintrel-tier3` are the daemon's own sessions.
    async fn session_start(&self, p: proto::SessionStart) -> Result<proto::SessionStarted, RpcError> {
        if p.rank_hint.is_some() || p.base.is_some() {
            return Err(RpcError::new(code::NOT_IMPLEMENTED, "rank_hint and base are not implemented yet"));
        }
        let safe = |c: char| c.is_ascii_alphanumeric() || "._-/@:+".contains(c);
        if p.agent.is_empty() || p.agent.len() > 128 || !p.agent.chars().all(safe) {
            return Err(RpcError::new(code::INVALID_PARAMS, "agent must be 1-128 characters from A-Z a-z 0-9 . _ - / @ : +"));
        }
        if RESERVED_AGENTS.contains(&p.agent.as_str()) {
            return Err(RpcError::new(code::INVALID_PARAMS, format!("agent {:?} is reserved for Kintrel itself", p.agent)));
        }
        self.open_session(p.agent).await
    }

    async fn open_session(&self, agent: String) -> Result<proto::SessionStarted, RpcError> {
        // Session ids double as σ: launch order (SPEC §4.5, open question Q1).
        let id = self.next_session.fetch_add(1, Ordering::Relaxed);
        let mut seed = Vec::new();
        seed.extend_from_slice(&self.started.to_le_bytes());
        seed.extend_from_slice(&std::process::id().to_le_bytes());
        seed.extend_from_slice(&id.to_le_bytes());
        // ponytail: derived token, fine behind the 0600 socket; signed identities arrive with server mode (§13).
        let token = Hash::of(&seed).to_string();
        let (base_root, base_parents) = self.with_reader(State::head_and_frontier).await?;
        let session = Session {
            id,
            agent: agent.clone(),
            rank: id,
            seq: 0,
            base_root,
            last_note: None,
            base_parents,
            last_gseq: 0,
            since_refresh: vec![],
            posts: (std::time::Instant::now(), 0),
            overlay: HashMap::new(),
            reads: BTreeMap::new(),
        };
        self.coord.lock().unwrap().start(id, agent);
        self.sessions.lock().unwrap().insert(token.clone(), Arc::new(tokio::sync::Mutex::new(session)));
        Ok(proto::SessionStarted { session: id, token, rank: id, base_root })
    }

    async fn read_file(&self, p: proto::ReadFile) -> Result<proto::FileContent, RpcError> {
        if !is_safe_rel_path(&p.path) {
            return Err(RpcError::new(code::INVALID_PARAMS, format!("unsafe path {:?}", p.path)));
        }
        // Reading at an explicit root needs no session: no overlay, not tracked.
        let session = match p.at {
            Some(_) => None,
            None => Some(self.session(&p.token)?),
        };
        let mut s = match &session {
            Some(s) => Some(s.lock().await),
            None => None,
        };
        let (root, overlay) = match (&s, p.at) {
            (Some(s), _) => (s.base_root, s.overlay.get(&p.path).cloned()),
            (None, at) => (at.unwrap_or_default(), None),
        };
        let path = p.path.clone();
        let tracked = p.at.is_none();
        let (file, atoms) = self
            .with_reader(move |st| {
                let file = view_file(st, root, overlay, &path)?;
                let atoms = match &file {
                    Some(f) if tracked => atom_hashes(&path, &f.bytes),
                    _ => vec![],
                };
                Ok((file, atoms))
            })
            .await?;
        let Some(file) = file else {
            return Err(RpcError::new(code::NOT_FOUND, format!("no file {:?} at {}", p.path, root.short())));
        };
        if let Some(s) = s.as_mut() {
            s.reads.extend(atoms);
            self.coord.lock().unwrap().touch(s.id);
            self.interested(s.id, s.base_root, [p.path.clone()]).await?;
        }
        let content = String::from_utf8(file.bytes)
            .map_err(|_| RpcError::new(code::INVALID_PARAMS, format!("{:?} is not UTF-8 text", p.path)))?;
        Ok(proto::FileContent { path: p.path, content, hash: file.id })
    }

    async fn post(&self, p: proto::Post) -> Result<proto::Posted, RpcError> {
        self.post_as(p, NoteKind::Code).await
    }

    /// Posts a note of `kind`. Only the daemon itself posts `Resolution` notes (Tier 3): a client
    /// could otherwise mark any conflict resolved.
    async fn post_as(&self, p: proto::Post, kind: NoteKind) -> Result<proto::Posted, RpcError> {
        let mut files = Vec::with_capacity(p.files.len());
        for f in p.files {
            let bytes = f.new_bytes().map_err(|e| RpcError::new(code::INVALID_PARAMS, e))?;
            files.push((f.path, bytes, f.base));
        }
        self.post_files(&p.token, files, kind, HashMap::new()).await
    }

    /// Posts `files` as (path, new content or `None` to delete, the hash the writer saw). For
    /// `kintrel sync` only, `from_git` gives per path the version a commit was made against and
    /// the file's new kind: that version, not the view's, is the op's base, so the combiner
    /// merges the commit into head like any other note.
    async fn post_files(
        &self,
        token: &str,
        files: Vec<(String, Option<Vec<u8>>, Option<Hash>)>,
        note_kind: NoteKind,
        from_git: HashMap<String, (Option<ViewFile>, Kind)>,
    ) -> Result<proto::Posted, RpcError> {
        let started = std::time::Instant::now();
        let invalid = |m: String| RpcError::new(code::INVALID_PARAMS, m);
        if files.is_empty() || files.len() > MAX_OPS {
            return Err(invalid(format!("a note must write 1..={MAX_OPS} files")));
        }
        let mut seen = HashSet::new();
        for (path, content, _) in &files {
            if !is_safe_rel_path(path) {
                return Err(invalid(format!("unsafe path {path:?}")));
            }
            if !seen.insert(path.as_str()) {
                return Err(invalid(format!("path {path:?} appears twice")));
            }
            if content.as_ref().is_some_and(|c| c.len() > MAX_BLOB) {
                return Err(invalid(format!("{path:?} exceeds {MAX_BLOB} bytes")));
            }
        }
        let kinds: HashMap<String, Kind> = from_git.iter().map(|(p, (_, k))| (p.clone(), *k)).collect();

        let session = self.session(token)?;
        let mut s = session.lock().await;
        if let Err(wait_ms) = allow_post(&mut s.posts, std::time::Instant::now()) {
            kintrel_core::metrics::POSTS_BUSY.add(1);
            return Err(RpcError::busy(wait_ms));
        }
        let (root, overlay) = (s.base_root, s.overlay.clone());
        // The view must stay a valid tree: a written file may not be a parent of another file,
        // or have one as a parent, among this post and the session's own writes. (Parents in
        // the base tree are checked against the store in `prepare`.)
        let mut after: HashSet<&str> = overlay.iter().filter(|(_, v)| v.is_some()).map(|(k, _)| k.as_str()).collect();
        for (path, content, _) in &files {
            if content.is_some() {
                after.insert(path);
            } else {
                after.remove(path.as_str());
            }
        }
        for (path, _, _) in files.iter().filter(|f| f.1.is_some()) {
            let clash = after.iter().find(|o| is_parent(path, o) || is_parent(o, path));
            if let Some(other) = clash {
                return Err(invalid(format!("{path:?} conflicts with file {other:?}")));
            }
        }
        let mut from_git = from_git;
        let prepared: Vec<Prepared> = self
            .with_reader(move |st| {
                files
                    .into_iter()
                    .map(|(path, content, base)| {
                        let against = from_git.remove(&path).map(|(b, _)| b);
                        prepare(st, root, &overlay, path, content, base, against)
                    })
                    .collect()
            })
            .await?;

        let mut ops = Vec::new();
        let mut blobs = Vec::new();
        let mut writes = Vec::new();
        let mut changes = Vec::new();
        for f in prepared {
            if let Some(why) = f.blocked {
                return Err(invalid(why));
            }
            let cur = f.current.as_ref().map(|c| c.id);
            if let Some(want) = f.base
                && cur.unwrap_or_default() != want
            {
                let current = f.current.map(|c| String::from_utf8_lossy(&c.bytes).into_owned());
                return Err(RpcError {
                    data: Some(serde_json::json!({ "path": f.path, "current_hash": cur, "current": current })),
                    ..RpcError::new(code::STALE_BASE, format!("{:?} changed since you read it", f.path))
                });
            }
            match (f.current, f.content) {
                (None, None) => return Err(invalid(format!("cannot delete {:?}: no such file", f.path))),
                (Some(c), None) => {
                    if kinds.contains_key(&f.path) {
                        blobs.push(NewBlob::of(&c.bytes));
                    }
                    ops.push(Op::Delete { path: f.path, base: c.id })
                }
                (cur, Some(new)) => {
                    let id = Hash::of(&new);
                    let kind = kinds.get(&f.path).copied().unwrap_or_else(|| cur.as_ref().map_or(Kind::File, |c| c.kind));
                    if cur.as_ref().is_some_and(|c| c.id == id && c.kind == kind) {
                        continue; // unchanged
                    }
                    if let Some(c) = cur.as_ref().filter(|_| kinds.contains_key(&f.path)) {
                        // A commit's base may be a version the board never stored.
                        blobs.push(NewBlob::of(&c.bytes));
                    }
                    blobs.push(NewBlob::of(&new));
                    ops.push(Op::Write { path: f.path, base: cur.map(|c| c.id), content: id, kind });
                }
            }
            writes.extend(f.atoms);
            changes.extend(f.changes);
        }
        if ops.is_empty() {
            return Err(invalid("the note changes nothing".into()));
        }

        let seq = s.seq + 1;
        let note = Note {
            author: s.agent.clone(),
            session: s.id,
            rank: s.rank,
            seq,
            base_root: root,
            parents: {
                let mut p: Vec<Hash> = s.last_note.into_iter().chain(s.base_parents.iter().copied()).collect();
                p.sort();
                p.dedup();
                p
            },
            kind: note_kind,
            ops,
            reads: s.reads.iter().map(|(key, &hash)| AtomRead { key: key.clone(), hash }).collect(),
        };
        let body = encode::encode(&note);
        let note_id = Hash::of(&body);
        let atoms: Vec<String> = writes.iter().map(ToString::to_string).collect();
        let gseq = self
            .log
            .append(NewNote {
                id: note_id,
                author: note.author,
                session: note.session,
                rank: note.rank,
                seq,
                kind: 0,
                body,
                blobs,
                writes: atoms.clone(),
                reads: note.reads.iter().map(|r| (r.key.to_string(), r.hash)).collect(),
            })
            .await
            .map_err(|e| match e {
                AppendError::Busy => {
                    kintrel_core::metrics::POSTS_BUSY.add(1);
                    RpcError::busy(1)
                }
                e => RpcError::new(code::INTERNAL, e.to_string()),
            })?;

        {
            let mut coord = self.coord.lock().unwrap();
            coord.touch(s.id);
            coord.posted(s.id, &note_id.short(), &writes);
            coord.wrote(s.id, &changes);
        }
        self.interested(s.id, root, note.ops.iter().map(|op| op.path().to_owned())).await?;
        self.combiner.wake();
        s.seq = seq;
        s.last_note = Some(note_id);
        s.base_parents.clear(); // later notes reach them through this one
        s.last_gseq = gseq;
        s.since_refresh.push(note_id);
        s.reads.clear();
        for op in note.ops {
            match op {
                Op::Write { path, content, kind, .. } => s.overlay.insert(path, Some((kind, content))),
                Op::Delete { path, .. } => s.overlay.insert(path, None),
            };
        }
        kintrel_core::metrics::observe_post(started.elapsed().as_micros() as u64);
        Ok(proto::Posted { note_id, gseq, atoms })
    }

    async fn refresh(&self, p: proto::Refresh) -> Result<proto::Refreshed, RpcError> {
        let session = self.session(&p.token)?;
        let mut s = session.lock().await;
        self.coord.lock().unwrap().touch(s.id);
        // Wait for a combiner pass that includes this session's last note (gseq is gapless and
        // every logged note is folded, so `folded` counts notes up to that gseq).
        let gseq = s.last_gseq;
        self.wait_heads(std::time::Duration::from_secs(10), |h| h.folded >= gseq).await?;
        let notes = s.since_refresh.clone();
        let (head, frontier, lost) = self
            .with_reader(move |st| {
                let (head, frontier) = st.head_and_frontier()?;
                let mut lost = vec![];
                for n in notes {
                    if matches!(st.note_state(n)?, Some(state::CONFLICTED | state::REJECTED)) {
                        lost.push(n);
                    }
                }
                Ok((head, frontier, lost))
            })
            .await?;
        s.base_root = head;
        s.base_parents = frontier;
        self.coord.lock().unwrap().refreshed(s.id);
        s.overlay.clear();
        s.since_refresh.clear();
        Ok(proto::Refreshed { base_root: head, lost })
    }

    async fn inbox(&self, p: proto::Inbox) -> Result<proto::Notices, RpcError> {
        let id = self.session(&p.token)?.lock().await.id;
        let mut coord = self.coord.lock().unwrap();
        coord.touch(id);
        Ok(proto::Notices { notices: coord.take_inbox(id) })
    }

    async fn announce(&self, p: proto::Announce, intent: bool) -> Result<proto::Announced, RpcError> {
        let invalid = |m: String| RpcError::new(code::INVALID_PARAMS, m);
        if p.summary.len() > MAX_SUMMARY {
            return Err(invalid(format!("summary exceeds {MAX_SUMMARY} bytes")));
        }
        if p.targets.is_empty() || p.targets.len() > MAX_OPS {
            return Err(invalid(format!("give 1..={MAX_OPS} target atoms")));
        }
        let mut targets = vec![];
        for t in &p.targets {
            let key: AtomKey = t.parse().map_err(|e| invalid(format!("bad atom key {t:?}: {e}")))?;
            if !is_safe_rel_path(&key.path) {
                return Err(invalid(format!("unsafe path {:?}", key.path)));
            }
            targets.push(key);
        }
        let id = self.session(&p.token)?.lock().await.id;
        let lease = p.lease_ms.map_or(notices::LEASE, std::time::Duration::from_millis);
        let mut coord = self.coord.lock().unwrap();
        coord.touch(id);
        let (id, overlapping) = coord.announce(id, intent, p.summary, targets, lease);
        Ok(proto::Announced { id, overlapping })
    }

    async fn release(&self, p: proto::Release) -> Result<proto::Status, RpcError> {
        let id = self.session(&p.token)?.lock().await.id;
        let mut coord = self.coord.lock().unwrap();
        coord.touch(id);
        match coord.release(id, p.id) {
            true => Ok(proto::Status {}),
            false => Err(RpcError::new(code::NOT_FOUND, format!("you hold no claim or intent {}", p.id))),
        }
    }

    async fn heartbeat(&self, p: proto::SessionRef) -> Result<proto::Status, RpcError> {
        let id = self.session(&p.token)?.lock().await.id;
        self.coord.lock().unwrap().touch(id);
        Ok(proto::Status {})
    }

    /// Ends a session: its claims, intents and inbox go; its notes stay (they are data).
    async fn end(&self, p: proto::SessionRef) -> Result<proto::Status, RpcError> {
        let id = self.session(&p.token)?.lock().await.id;
        self.sessions.lock().unwrap().remove(&p.token);
        self.coord.lock().unwrap().end(id);
        Ok(proto::Status {})
    }

    /// Adds files to a session's footprint. For a file new to it, the session also gets
    /// notices for what others changed there between its base and head ([`Coord::catch_up`]).
    async fn interested(&self, session: u64, base: Hash, paths: impl IntoIterator<Item = String>) -> Result<(), RpcError> {
        let new = self.coord.lock().unwrap().add_paths(session, paths);
        if new.is_empty() {
            return Ok(());
        }
        let head = self.heads.borrow().head;
        let paths = new.clone();
        let changes = if head == base {
            vec![]
        } else {
            self.with_reader(move |st| {
                let mut out = vec![];
                for path in new {
                    let file = |root| -> kintrel_core::Result<Option<Vec<u8>>> {
                        Ok(view_file(st, root, None, &path)?.map(|f| f.bytes))
                    };
                    let (old, now) = (file(base)?, file(head)?);
                    if old != now {
                        out.extend(diff_atoms(&path, old.as_deref(), now.as_deref()));
                    }
                }
                Ok(out)
            })
            .await?
        };
        self.coord.lock().unwrap().catch_up(session, &paths, &changes);
        Ok(())
    }

    /// Promotes head once every note logged so far is folded, unless the gate blocks it.
    async fn promote_now(&self, p: proto::PromoteNow) -> Result<proto::StatusInfo, RpcError> {
        if let Some(reviewed) = p.approve_config {
            let (head, promoted) = {
                let h = self.heads.borrow();
                (h.head, h.promoted)
            };
            let (theirs, trusted) = self
                .with_reader(move |st| {
                    let of = |r| kintrel_ci::config_of(st, r).map_err(|e| kintrel_core::Error::Invalid(e.to_string()));
                    Ok((of(head)?, of(promoted)?))
                })
                .await?;
            if theirs != Some(reviewed) || theirs == trusted {
                return Err(RpcError::new(
                    code::INVALID_PARAMS,
                    "head's kintrel.toml is not the one you reviewed (or is unchanged); review it again",
                ));
            }
            if p.approval != Some(board::approval(&self.person_key, trusted, reviewed)) {
                return Err(RpcError::new(code::INVALID_PARAMS, "approving kintrel.toml needs the person key"));
            }
            self.worker.approve_config(trusted, theirs);
        }
        let logged = self.log.last_gseq();
        let asked = self.heads.borrow().passes;
        self.combiner.promote_now();
        // Only a pass that ran after this request answers it. Gate checks may take minutes
        // (SPEC §10.5 timeouts bound them).
        self.wait_heads(std::time::Duration::from_secs(900), |h| {
            h.passes > asked && h.folded >= logged && (h.promoted == h.head || !h.blocked.is_empty() || !h.failing.is_empty())
        })
        .await?;
        Ok(self.status())
    }

    /// Waits up to `limit` for the combiner to publish heads that satisfy `done`.
    async fn wait_heads(&self, limit: std::time::Duration, done: impl FnMut(&Heads) -> bool) -> Result<(), RpcError> {
        match tokio::time::timeout(limit, self.heads.clone().wait_for(done)).await {
            Ok(Ok(_)) => Ok(()),
            _ => Err(RpcError::new(code::INTERNAL, "the combiner is not keeping up; try again")),
        }
    }

    /// Prometheus text: the process-wide counters, and the status figures as gauges.
    fn metrics(&self) -> String {
        let s = self.status();
        let passes = self.heads.borrow().passes;
        kintrel_core::metrics::render(&[
            ("sessions", "Live sessions.", s.sessions),
            ("notes", "Notes in the log.", s.notes),
            ("combiner_lag_notes", "Notes logged but not yet folded into head.", s.notes.saturating_sub(s.folded)),
            ("unpromoted_notes", "Notes in head not yet promoted.", s.unpromoted),
            ("promotion_wait_ms", "How long promotion has waited for them.", s.promotion_wait_ms),
            ("conflicts_open", "Open conflicts.", s.conflicts),
            ("ci_pending", "Check runs queued or running, and deploys waiting.", s.ci_pending),
            ("combiner_passes", "Combiner passes so far.", passes),
            ("combiner_last_pass_ms", "Duration of the latest combiner pass.", s.pass_ms),
        ])
    }

    fn status(&self) -> proto::StatusInfo {
        let h = self.heads.borrow().clone();
        proto::StatusInfo {
            sessions: self.sessions.lock().unwrap().len() as u64,
            notes: self.log.last_gseq(),
            head: Some(h.head),
            promoted: Some(h.promoted),
            conflicts: h.conflicts,
            folded: h.folded,
            unpromoted: h.folded.saturating_sub(h.promoted_gseq),
            promotion_wait_ms: match h.folded > h.promoted_gseq && h.promoted_ms > 0 {
                true => kintrel_core::now_ms().saturating_sub(h.promoted_ms),
                false => 0,
            },
            ci_pending: self.worker.pending(),
            pass_ms: h.pass_ms,
            blocked: h.blocked,
            failing: h.failing,
        }
    }

    fn session(&self, token: &str) -> Result<Arc<tokio::sync::Mutex<Session>>, RpcError> {
        self.sessions
            .lock()
            .unwrap()
            .get(token)
            .cloned()
            .ok_or_else(|| RpcError::new(code::UNAUTHORIZED, "unknown or expired session token"))
    }

    /// Runs a blocking store read on a pooled connection.
    async fn with_reader<T: Send + 'static>(
        &self,
        f: impl FnOnce(&State) -> kintrel_core::Result<T> + Send + 'static,
    ) -> Result<T, RpcError> {
        let internal = |e: &dyn std::fmt::Display| RpcError::new(code::INTERNAL, e.to_string());
        let pooled = self.readers.lock().unwrap().pop();
        let st = match pooled {
            Some(st) => st,
            None => open_reader(&self.repo).map_err(|e| internal(&e))?,
        };
        let (st, out) = tokio::task::spawn_blocking(move || {
            let out = f(&st);
            (st, out)
        })
        .await
        .map_err(|e| internal(&e))?;
        self.readers.lock().unwrap().push(st);
        out.map_err(|e| internal(&e))
    }
}

fn open_reader(repo: &Path) -> kintrel_core::Result<State> {
    State::open_board(repo)
}

/// The file at `path` in a view: the session's own write if it has one, else the base tree.
fn view_file(
    st: &State,
    root: Hash,
    own: Option<Option<(Kind, Hash)>>,
    path: &str,
) -> kintrel_core::Result<Option<ViewFile>> {
    let entry = match own {
        Some(own) => own,
        None => st.lookup(root, path)?.filter(|(kind, _)| *kind != Kind::Dir),
    };
    let Some((kind, id)) = entry else { return Ok(None) };
    let bytes = st.blob(id)?;
    Ok(Some(ViewFile { kind, id, bytes }))
}

fn prepare(
    st: &State,
    root: Hash,
    overlay: &HashMap<String, Option<(Kind, Hash)>>,
    path: String,
    content: Option<Vec<u8>>,
    base: Option<Hash>,
    against: Option<Option<ViewFile>>,
) -> kintrel_core::Result<Prepared> {
    let mut blocked = None;
    // Every parent must be a directory (or absent) in the view, and the path must not be one.
    let mut prefix = String::new();
    for seg in path.split('/').collect::<Vec<_>>().split_last().map_or(&[][..], |(_, parents)| parents) {
        prefix = if prefix.is_empty() { (*seg).to_owned() } else { format!("{prefix}/{seg}") };
        let own_file = matches!(overlay.get(&prefix), Some(Some(_)));
        let base_file = !overlay.contains_key(&prefix) && st.lookup(root, &prefix)?.is_some_and(|(k, _)| k != Kind::Dir);
        if own_file || base_file {
            blocked = Some(format!("{path:?}: {prefix:?} is a file"));
        }
    }
    if !overlay.contains_key(&path) && st.lookup(root, &path)?.is_some_and(|(k, _)| k == Kind::Dir) {
        blocked = Some(format!("{path:?} is a directory"));
    }
    let current = match against {
        Some(version) => version,
        None => view_file(st, root, overlay.get(&path).cloned(), &path)?,
    };
    let changes = diff_atoms(&path, current.as_ref().map(|c| &c.bytes[..]), content.as_deref());
    let mut atoms: Vec<AtomKey> = changes.iter().map(|c| c.atom.clone()).collect();
    atoms.sort();
    Ok(Prepared { path, current, content, base, blocked, atoms, changes })
}

/// Counts a post against its session's one-second window; `Err` is how long to wait.
fn allow_post(window: &mut (std::time::Instant, u32), now: std::time::Instant) -> Result<(), u64> {
    let elapsed = now.duration_since(window.0);
    if elapsed >= std::time::Duration::from_secs(1) {
        *window = (now, 0);
    } else if window.1 >= proto::MAX_POSTS_PER_SEC {
        return Err(1000 - elapsed.as_millis() as u64);
    }
    window.1 += 1;
    Ok(())
}

/// `parent` is a strict ancestor directory of `path`.
fn is_parent(parent: &str, path: &str) -> bool {
    path.len() > parent.len() && path.starts_with(parent) && path.as_bytes()[parent.len()] == b'/'
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::new(code::INVALID_PARAMS, e.to_string()))
}

fn reply<T: Serialize>(r: Result<T, RpcError>) -> Result<Value, RpcError> {
    r.map(|v| serde_json::to_value(v).expect("results always serialize"))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    #[test]
    fn posts_are_limited_per_second() {
        let start = Instant::now();
        let mut window = (start, 0);
        for _ in 0..kintrel_core::proto::MAX_POSTS_PER_SEC {
            assert!(super::allow_post(&mut window, start).is_ok());
        }
        let wait = super::allow_post(&mut window, start + Duration::from_millis(400)).unwrap_err();
        assert_eq!(wait, 600, "the rest of the second");
        assert!(super::allow_post(&mut window, start + Duration::from_secs(1)).is_ok(), "a new window");
        assert_eq!(window.1, 1);
    }
}
