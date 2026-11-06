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
use kintrel_core::merge::merge3;
use kintrel_core::note::{AtomKey, AtomRead, Note, NoteKind, Op, atom_hashes};
use kintrel_core::state::{self, State};
use kintrel_core::tree::Kind;
use kintrel_core::{Hash, blob, board, encode};
use kintrel_core::log::{AppendError, Durability, LogWriter, NewBlob, NewNote};
use kintrel_core::path::is_safe_rel_path;
use kintrel_core::proto::{self, MAX_BLOB, MAX_OPS, MAX_SUMMARY, Request, Response, RpcError, code};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::notices::{self, Coord, Interest};
use crate::rpc::{read_frame, write_frame};

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

const TIER3_AGENT: &str = "kintrel-tier3";
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
    for _ in 1..std::thread::available_parallelism().map_or(4, |n| n.get()) {
        readers.push(open_reader(&opts.repo).map_err(io::Error::other)?);
    }
    let head = reader.get_ref(state::HEAD).map_err(io::Error::other)?.ok_or_else(not_board)?;
    let promoted = reader.get_ref(state::PROMOTED).map_err(io::Error::other)?.ok_or_else(not_board)?;
    let first_session = reader.max_session().map_err(io::Error::other)? + 1;
    readers.push(reader);

    let heads = Arc::new(Mutex::new(Heads { head, promoted, ..Heads::default() }));
    let (events, inbound) = std::sync::mpsc::channel();
    let ci = Arc::new(kintrel_ci::Ci::open(&opts.repo).map_err(io::Error::other)?);
    let worker = crate::ci_worker::CiWorker::start(opts.repo.clone(), ci.clone())?;
    let gate: Arc<dyn kintrel_core::combine::Gate> = worker.clone();
    let combiner = Combiner::spawn(&opts.repo, heads.clone(), opts.promote_every, Some(events), Some(gate)).map_err(io::Error::other)?;
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
                eprintln!("kintrel tier3: {}: {}", conflict.atom, e.message);
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
            notices::route(&mut router.coord.lock().unwrap(), events);
            if applied {
                worker.feedback(router.heads.lock().unwrap().head);
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
}

struct Board {
    repo: PathBuf,
    /// Idle read connections (state.db with log.db attached); `with_reader` opens more on demand.
    readers: Mutex<Vec<State>>,
    /// Published by the combiner after each pass.
    heads: Arc<Mutex<Heads>>,
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
    async fn dispatch(&self, method: &str, params: Value) -> Result<Value, RpcError> {
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
        let (base_root, base_parents) = self.with_reader(|st| st.head_and_frontier()).await?;
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
        let files = p.files.into_iter().map(|f| (f.path, f.content.map(String::into_bytes), f.base)).collect();
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
                        blobs.push(NewBlob { id: c.id, len: c.bytes.len() as u64, zstd: blob::compress(&c.bytes) });
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
                        blobs.push(NewBlob { id: c.id, len: c.bytes.len() as u64, zstd: blob::compress(&c.bytes) });
                    }
                    blobs.push(NewBlob { id, len: new.len() as u64, zstd: blob::compress(&new) });
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
                AppendError::Busy => RpcError::busy(1),
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
        Ok(proto::Posted { note_id, gseq, atoms })
    }

    async fn refresh(&self, p: proto::Refresh) -> Result<proto::Refreshed, RpcError> {
        let session = self.session(&p.token)?;
        let mut s = session.lock().await;
        self.coord.lock().unwrap().touch(s.id);
        // Wait for a combiner pass that includes this session's last note (gseq is gapless and
        // every logged note is folded, so `folded` counts notes up to that gseq).
        let start = std::time::Instant::now();
        loop {
            if self.heads.lock().unwrap().folded >= s.last_gseq {
                break;
            }
            if start.elapsed() > std::time::Duration::from_secs(10) {
                return Err(RpcError::new(code::INTERNAL, "the combiner is not keeping up; try again"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
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
        let notices = coord.sessions.get_mut(&id).map(Interest::take).unwrap_or_default();
        Ok(proto::Notices { notices })
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

    /// Reverts a note as a three-way merge per file: base = the note's result, ours = the view,
    /// theirs = the note's base. Only the note's own atoms go back; later work stays.
    async fn revert(&self, p: proto::Revert) -> Result<proto::Posted, RpcError> {
        let (root, overlay) = {
            let s = self.session(&p.token)?;
            let s = s.lock().await;
            (s.base_root, s.overlay.clone())
        };
        let prefix = p.note.clone();
        let files = self
            .with_reader(move |st| {
                let Some((_, note)) = st.find_note(&prefix)? else { return Ok(Err(format!("no single note matches {prefix:?}"))) };
                let blob = |id: Option<Hash>| -> kintrel_core::Result<Option<Vec<u8>>> {
                    id.map(|id| st.get_blob(id)?.ok_or_else(|| kintrel_core::Error::Corrupt(format!("missing blob {id}")))).transpose()
                };
                let mut out = vec![];
                let mut stuck = vec![];
                for op in &note.ops {
                    let (before, after) = match op {
                        Op::Write { base, content, .. } => (blob(*base)?, blob(Some(*content))?),
                        Op::Delete { base, .. } => (blob(Some(*base))?, None),
                    };
                    let view = view_file(st, root, overlay.get(op.path()).cloned(), op.path())?.map(|f| f.bytes);
                    match merge3(op.path(), after.as_deref(), view.as_deref(), before.as_deref()) {
                        Ok(r) if r == view => {}
                        Ok(r) => out.push((op.path().to_owned(), r)),
                        Err(atoms) => stuck.extend(atoms.into_iter().map(|a| a.to_string())),
                    }
                }
                Ok(if stuck.is_empty() { Ok(out) } else { Err(stuck.join(", ")) })
            })
            .await?;
        let files = match files {
            Ok(f) if f.is_empty() => return Err(RpcError::new(code::INVALID_PARAMS, "nothing to revert: your view already lacks that note's changes")),
            Ok(f) => f,
            Err(why) if why.starts_with("no single note") => return Err(RpcError::new(code::NOT_FOUND, why)),
            Err(atoms) => {
                return Err(RpcError {
                    data: Some(serde_json::json!({ "atoms": atoms.split(", ").collect::<Vec<_>>() })),
                    ..RpcError::new(code::CONFLICT, format!("cannot revert cleanly: {atoms} changed since"))
                });
            }
        };
        let mut writes = vec![];
        for (path, content) in files {
            let content = content
                .map(String::from_utf8)
                .transpose()
                .map_err(|_| RpcError::new(code::INVALID_PARAMS, format!("{path:?} is not UTF-8 text")))?;
            writes.push(proto::FileWrite { path, content, base: None });
        }
        self.post(proto::Post { token: p.token, files: writes, summary: Some(format!("revert {}", p.note)) }).await
    }

    /// Runs checks on a session's view (its base plus its own writes), a root, or head.
    async fn checks(&self, p: proto::ChecksRun) -> Result<proto::ChecksRan, RpcError> {
        let tier = match p.tier.as_deref() {
            None => None,
            Some("lint") => Some(kintrel_ci::Tier::Lint),
            Some("typecheck") => Some(kintrel_ci::Tier::Typecheck),
            Some("test") => Some(kintrel_ci::Tier::Test),
            Some(t) => return Err(RpcError::new(code::INVALID_PARAMS, format!("unknown tier {t:?}"))),
        };
        let snap = match (&p.token, p.at) {
            (Some(token), _) => {
                let s = self.session(token)?;
                let s = s.lock().await;
                self.coord.lock().unwrap().touch(s.id);
                kintrel_ci::Snapshot { root: s.base_root, overlay: s.overlay.clone().into_iter().collect(), ..Default::default() }
            }
            (None, Some(root)) => kintrel_ci::Snapshot { root, ..Default::default() },
            (None, None) => kintrel_ci::Snapshot { root: self.heads.lock().unwrap().head, ..Default::default() },
        };
        let (ci, repo, root) = (self.ci.clone(), self.repo.clone(), snap.root);
        let promoted = self.heads.lock().unwrap().promoted;
        let outcomes = tokio::task::spawn_blocking(move || -> Result<Vec<kintrel_ci::Outcome>, String> {
            let st = open_reader(&repo).map_err(|e| e.to_string())?;
            // The promoted config, never the snapshot's own (see ci_worker).
            let config = kintrel_ci::config_of(&st, promoted).map_err(|e| e.to_string())?;
            ci.run(&st, &repo, config, &snap, tier).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| RpcError::new(code::INTERNAL, e.to_string()))?
        .map_err(|e| RpcError::new(code::INVALID_PARAMS, e))?;
        let outcomes = outcomes
            .into_iter()
            .map(|o| proto::CheckOutcome {
                check: o.check,
                tier: format!("{:?}", o.tier).to_lowercase(),
                target: o.target,
                ok: o.ok,
                cached: o.cached,
                timed_out: o.timed_out,
                ms: o.ms,
                output: o.output,
                key: o.key,
            })
            .collect();
        Ok(proto::ChecksRan { root, outcomes })
    }

    /// Tier 3 (SPEC §7.3): asks Claude to merge a conflicting note into current head, and posts
    /// the validated result as a `Resolution` note from the `kintrel-tier3` session, where the
    /// gate judges it like any note. Only when the trusted (promoted) kintrel.toml enables it.
    async fn tier3(&self, losing_session: u64, conflict: &kintrel_core::combine::Conflict, token: &mut Option<String>) -> Result<(), RpcError> {
        let internal = |m: String| RpcError::new(code::INTERNAL, m);
        let promoted = self.heads.lock().unwrap().promoted;
        let settings = self
            .with_reader(move |st| {
                let Some(id) = kintrel_ci::config_of(st, promoted).map_err(|e| kintrel_core::Error::Invalid(e.to_string()))? else { return Ok(None) };
                let text = st.get_blob(id)?.unwrap_or_default();
                Ok(kintrel_ci::Config::parse(&String::from_utf8_lossy(&text)).ok().map(|c| c.merge))
            })
            .await?;
        let Some(settings) = settings.filter(|m| m.tier3) else { return Ok(()) };
        if settings.tier3_command.is_none() && !kintrel_core::tier3::available() {
            return Err(internal("no backend: set ANTHROPIC_API_KEY or install the claude CLI".into()));
        }
        let model = settings.tier3_model.unwrap_or_else(|| kintrel_core::tier3::DEFAULT_MODEL.into());
        let command = settings.tier3_command;
        if token.is_none() {
            let started = self.open_session(TIER3_AGENT.into()).await?;
            *token = Some(started.token);
        }
        let token = token.clone().expect("set above");
        // Merge into the current head.
        let _: proto::Refreshed = self.refresh(proto::Refresh { token: token.clone() }).await?;
        let (path, losing) = (conflict.atom.path.clone(), conflict.losing.to_string());
        let ours = self.read_file(proto::ReadFile { token: token.clone(), path: path.clone(), at: None }).await?;
        let p = path.clone();
        let sides = self
            .with_reader(move |st| {
                let Some((_, note)) = st.find_note(&losing)? else { return Ok(None) };
                let blob = |id: Option<Hash>| -> kintrel_core::Result<Vec<u8>> {
                    Ok(id.map(|id| st.get_blob(id)).transpose()?.flatten().unwrap_or_default())
                };
                Ok(match note.ops.iter().find(|op| op.path() == p) {
                    Some(Op::Write { base, content, .. }) => Some((blob(*base)?, blob(Some(*content))?)),
                    _ => None, // a deletion against an edit: nothing to merge textually
                })
            })
            .await?;
        let Some((base, theirs)) = sides else { return Ok(()) };
        let ours_bytes = ours.content.clone().into_bytes();
        let p = path.clone();
        let merged = tokio::task::spawn_blocking(move || {
            let merged = kintrel_core::tier3::propose_with(&p, &base, &ours_bytes, &theirs, &model, command.as_deref())?;
            if !kintrel_core::merge::proposal_keeps_sides(&p, &base, &ours_bytes, &theirs, &merged) {
                return Err("the proposal changed one-sided edits or does not parse".to_string());
            }
            String::from_utf8(merged).map_err(|_| "the proposal is not UTF-8".to_string())
        })
        .await
        .map_err(|e| internal(e.to_string()))?
        .map_err(internal)?;
        let posted = self
            .post_as(
                proto::Post {
                    token,
                    files: vec![proto::FileWrite { path, content: Some(merged), base: Some(ours.hash) }],
                    summary: Some(format!("Tier 3 merge for conflict {}", conflict.id().short())),
                },
                NoteKind::Resolution { conflict: conflict.id() },
            )
            .await?;
        self.coord.lock().unwrap().merge_proposed(losing_session, &conflict.atom, posted.note_id);
        Ok(())
    }

    /// Brings a Git branch's new commits into the board (SPEC §11): each one becomes notes from
    /// its own `git` session, made against the commit's base, so the combiner merges it into head
    /// and conflicts with agents' work are recorded like any other. The sync mark moves past
    /// each commit once its notes are folded.
    async fn sync(&self, p: proto::Sync) -> Result<proto::Synced, RpcError> {
        let invalid = |e: anyhow::Error| RpcError::new(code::INVALID_PARAMS, format!("{e:#}"));
        let internal = |e: &dyn std::fmt::Display| RpcError::new(code::INTERNAL, e.to_string());
        let repo = self.repo.clone();
        let (branch, commits) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let branch = match p.branch {
                Some(b) => b,
                None => crate::git::head_branch(&repo)?,
            };
            let commits = crate::git::pending(&repo, &branch, p.since.as_deref())?;
            Ok((branch, commits))
        })
        .await
        .map_err(|e| internal(&e))?
        .map_err(invalid)?;
        let mut synced = vec![];
        for c in commits {
            let started = self.open_session(crate::git::SYNC_AGENT.into()).await?;
            let mut notes = vec![];
            let mut result = Ok(());
            for chunk in c.files.chunks(MAX_OPS) {
                let mut from_git = HashMap::new();
                let mut files = vec![];
                for f in chunk {
                    let base = f.base.as_ref().map(|(kind, bytes)| ViewFile { kind: *kind, id: Hash::of(bytes), bytes: bytes.clone() });
                    from_git.insert(f.path.clone(), (base, f.new.as_ref().map_or(Kind::File, |(k, _)| *k)));
                    files.push((f.path.clone(), f.new.as_ref().map(|(_, b)| b.clone()), None));
                }
                match self.post_files(&started.token, files, NoteKind::Code, from_git).await {
                    Ok(posted) => notes.push(posted.note_id),
                    Err(e) => {
                        result = Err(e);
                        break;
                    }
                }
            }
            let lost = match result {
                Ok(()) => self.refresh(proto::Refresh { token: started.token.clone() }).await.map(|r| r.lost),
                Err(e) => Err(e),
            };
            self.end(proto::SessionRef { token: started.token }).await?;
            let lost = lost.map_err(|e| RpcError::new(e.code, format!("commit {}: {}", c.id, e.message)))?;
            let (repo, branch, id) = (self.repo.clone(), branch.clone(), c.id);
            tokio::task::spawn_blocking(move || crate::git::set_sync_mark(&repo, &branch, id))
                .await
                .map_err(|e| internal(&e))?
                .map_err(|e| internal(&format!("{e:#}")))?;
            synced.push(proto::SyncedCommit { commit: c.id.to_string(), summary: c.summary, notes, lost });
        }
        Ok(proto::Synced { branch, commits: synced })
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
    /// notices for what others changed there between its base and head ([`notices::catch_up`]).
    async fn interested(&self, session: u64, base: Hash, paths: impl IntoIterator<Item = String>) -> Result<(), RpcError> {
        let new: Vec<String> = match self.coord.lock().unwrap().sessions.get_mut(&session) {
            Some(int) => paths.into_iter().filter(|p| int.paths.insert(p.clone())).collect(),
            None => return Ok(()),
        };
        if new.is_empty() {
            return Ok(());
        }
        let head = self.heads.lock().unwrap().head;
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
        notices::catch_up(&mut self.coord.lock().unwrap(), session, &paths, &changes);
        Ok(())
    }

    /// Promotes head once every note logged so far is folded, unless the gate blocks it.
    async fn promote_now(&self, p: proto::PromoteNow) -> Result<proto::StatusInfo, RpcError> {
        if let Some(reviewed) = p.approve_config {
            let (head, promoted) = {
                let h = self.heads.lock().unwrap();
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
        let asked = self.heads.lock().unwrap().passes;
        self.combiner.promote_now();
        let start = std::time::Instant::now();
        loop {
            let h = self.heads.lock().unwrap().clone();
            // Only a pass that ran after this request answers it.
            if h.passes > asked && h.folded >= logged && (h.promoted == h.head || !h.blocked.is_empty() || !h.failing.is_empty()) {
                return Ok(self.status());
            }
            // Gate checks may take minutes (SPEC §10.5 timeouts bound them).
            if start.elapsed() > std::time::Duration::from_secs(900) {
                return Err(RpcError::new(code::INTERNAL, "the combiner is not keeping up; try again"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    fn status(&self) -> proto::StatusInfo {
        let h = self.heads.lock().unwrap().clone();
        proto::StatusInfo {
            sessions: self.sessions.lock().unwrap().len() as u64,
            notes: self.log.last_gseq(),
            head: Some(h.head),
            promoted: Some(h.promoted),
            conflicts: h.conflicts,
            folded: h.folded,
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
    State::open_with_log(&board::state_db(repo), &board::log_db(repo))
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
    let bytes = st.get_blob(id)?.ok_or_else(|| kintrel_core::Error::Corrupt(format!("missing blob {id}")))?;
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
