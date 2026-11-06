//! `kintreld`: one daemon per board, serving JSON-RPC on `.kintrel/kintrel.sock` (SPEC §3, §12.1).
//!
//! Still partly a stub: sessions live in memory, `post` stores whole files as a stub note in the
//! real group-commit log, and reads see only the session's base root (no own-note overlay yet).

use std::collections::{HashMap, HashSet};
use std::fs::{self, Permissions};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use kintrel_core::state::{self, State};
use kintrel_core::tree::Kind;
use kintrel_core::{Hash, blob, board};
use kintrel_core::log::{AppendError, Durability, LogWriter, NewBlob, NewNote};
use kintrel_core::path::is_safe_rel_path;
use kintrel_core::proto::{self, MAX_BLOB, MAX_OPS, Request, Response, RpcError, code};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::net::{UnixListener, UnixStream};

use crate::rpc::{read_frame, write_frame};

pub struct Options {
    pub repo: PathBuf,
    pub durability: Durability,
}

pub fn socket_path(repo: &Path) -> PathBuf {
    board::data_dir(repo).join("kintrel.sock")
}

/// Runs the daemon until the process exits.
pub async fn run(opts: Options) -> io::Result<()> {
    let not_board = || io::Error::new(io::ErrorKind::NotFound, format!("{} is not a kintrel board (run `kintrel init`)", opts.repo.display()));
    if !board::state_db(&opts.repo).exists() {
        return Err(not_board());
    }
    let reader = State::open(&board::state_db(&opts.repo)).map_err(io::Error::other)?;
    let head = reader.get_ref(state::HEAD).map_err(io::Error::other)?.ok_or_else(not_board)?;
    let promoted = reader.get_ref(state::PROMOTED).map_err(io::Error::other)?.ok_or_else(not_board)?;
    let sock = socket_path(&opts.repo);
    if UnixStream::connect(&sock).await.is_ok() {
        return Err(io::Error::new(io::ErrorKind::AddrInUse, "a daemon is already running for this board"));
    }
    match fs::remove_file(&sock) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let log = LogWriter::open(&board::log_db(&opts.repo), opts.durability).map_err(io::Error::other)?;
    let listener = UnixListener::bind(&sock)?;
    fs::set_permissions(&sock, Permissions::from_mode(0o600))?;

    let board = Arc::new(Board {
        repo: opts.repo,
        readers: Mutex::new(vec![reader]),
        refs: Mutex::new((head, promoted)),
        log,
        sessions: Mutex::new(HashMap::new()),
        next_session: AtomicU64::new(1),
        started: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos(),
    });
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(serve(board.clone(), stream));
    }
}

async fn serve(board: Arc<Board>, mut stream: UnixStream) {
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
}

struct Board {
    repo: PathBuf,
    /// Idle read connections to state.db; `with_reader` opens more on demand.
    readers: Mutex<Vec<State>>,
    /// (head, promoted). The combiner will move these; for now they are the init root.
    refs: Mutex<(Hash, Hash)>,
    log: LogWriter,
    // A per-session async lock serializes one session's posts, which keeps `seq` gapless.
    sessions: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Session>>>>,
    next_session: AtomicU64,
    started: u128,
}

impl Board {
    async fn dispatch(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            proto::SESSION_START => reply(self.session_start(parse(params)?)),
            proto::POST => reply(self.post(parse(params)?).await),
            proto::STATUS => reply(Ok(self.status())),
            proto::READ_FILE => reply(self.read_file(parse(params)?).await),
            _ => Err(RpcError::new(code::METHOD_NOT_FOUND, format!("unknown method {method:?}"))),
        }
    }

    fn session_start(&self, p: proto::SessionStart) -> Result<proto::SessionStarted, RpcError> {
        if p.rank_hint.is_some() || p.base.is_some() {
            return Err(RpcError::new(code::NOT_IMPLEMENTED, "rank_hint and base are not implemented yet"));
        }
        if p.agent.is_empty() {
            return Err(RpcError::new(code::INVALID_PARAMS, "agent must not be empty"));
        }
        // Session ids double as σ: launch order (SPEC §4.5, open question Q1).
        let id = self.next_session.fetch_add(1, Ordering::Relaxed);
        let mut seed = Vec::new();
        seed.extend_from_slice(&self.started.to_le_bytes());
        seed.extend_from_slice(&std::process::id().to_le_bytes());
        seed.extend_from_slice(&id.to_le_bytes());
        // ponytail: derived token, fine behind the 0600 socket; signed identities arrive with server mode (§13).
        let token = Hash::of(&seed).to_string();
        let base_root = self.refs.lock().unwrap().0;
        let session = Session { id, agent: p.agent, rank: id, seq: 0, base_root };
        self.sessions.lock().unwrap().insert(token.clone(), Arc::new(tokio::sync::Mutex::new(session)));
        Ok(proto::SessionStarted { session: id, token, rank: id, base_root })
    }

    async fn post(&self, p: proto::Post) -> Result<proto::Posted, RpcError> {
        let invalid = |m: String| RpcError::new(code::INVALID_PARAMS, m);
        if p.files.is_empty() || p.files.len() > MAX_OPS {
            return Err(invalid(format!("a note must write 1..={MAX_OPS} files")));
        }
        let mut seen = HashSet::new();
        for f in &p.files {
            if !is_safe_rel_path(&f.path) {
                return Err(invalid(format!("unsafe path {:?}", f.path)));
            }
            if !seen.insert(f.path.as_str()) {
                return Err(invalid(format!("path {:?} appears twice", f.path)));
            }
            if f.content.len() > MAX_BLOB {
                return Err(invalid(format!("{:?} exceeds {MAX_BLOB} bytes", f.path)));
            }
        }

        let session = self.session(&p.token)?;
        let mut s = session.lock().await;
        let seq = s.seq + 1;

        let blobs: Vec<NewBlob> = p
            .files
            .iter()
            .map(|f| NewBlob {
                id: Hash::of(f.content.as_bytes()),
                len: f.content.len() as u64,
                zstd: blob::compress(f.content.as_bytes()),
            })
            .collect();
        // ponytail: stub note body (JSON of path → blob); the canonical postcard Note replaces it in Phase 1.
        let body = serde_json::to_vec(&StubNote {
            session: s.id,
            rank: s.rank,
            seq,
            files: p.files.iter().zip(&blobs).map(|(f, b)| (f.path.as_str(), b.id)).collect(),
            summary: p.summary.as_deref(),
        })
        .expect("stub note serializes");
        let note = NewNote {
            id: Hash::of(&body),
            author: s.agent.clone(),
            session: s.id,
            rank: s.rank,
            seq,
            kind: 0,
            body,
            blobs,
        };
        let note_id = note.id;
        let gseq = self.log.append(note).await.map_err(|e| match e {
            AppendError::Busy => RpcError::busy(1),
            e => RpcError::new(code::INTERNAL, e.to_string()),
        })?;
        s.seq = seq;
        Ok(proto::Posted { note_id, gseq })
    }

    async fn read_file(&self, p: proto::ReadFile) -> Result<proto::FileContent, RpcError> {
        let root = match p.at {
            Some(at) => at,
            None => self.session(&p.token)?.lock().await.base_root,
        };
        if !is_safe_rel_path(&p.path) {
            return Err(RpcError::new(code::INVALID_PARAMS, format!("unsafe path {:?}", p.path)));
        }
        let path = p.path.clone();
        let found = self
            .with_reader(move |st| {
                let Some((kind, id)) = st.lookup(root, &path)? else { return Ok(None) };
                if kind == Kind::Dir {
                    return Ok(None);
                }
                Ok(st.get_blob(id)?.map(|b| (id, b)))
            })
            .await?;
        let Some((hash, bytes)) = found else {
            return Err(RpcError::new(code::NOT_FOUND, format!("no file {:?} at {}", p.path, root.short())));
        };
        let content = String::from_utf8(bytes)
            .map_err(|_| RpcError::new(code::INVALID_PARAMS, format!("{:?} is not UTF-8 text", p.path)))?;
        Ok(proto::FileContent { path: p.path, content, hash })
    }

    fn status(&self) -> proto::StatusInfo {
        let (head, promoted) = *self.refs.lock().unwrap();
        proto::StatusInfo {
            sessions: self.sessions.lock().unwrap().len() as u64,
            notes: self.log.last_gseq(),
            head: Some(head),
            promoted: Some(promoted),
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

    /// Runs a blocking store read on a pooled state.db connection.
    async fn with_reader<T: Send + 'static>(
        &self,
        f: impl FnOnce(&State) -> kintrel_core::Result<T> + Send + 'static,
    ) -> Result<T, RpcError> {
        let internal = |e: &dyn std::fmt::Display| RpcError::new(code::INTERNAL, e.to_string());
        let pooled = self.readers.lock().unwrap().pop();
        let st = match pooled {
            Some(st) => st,
            None => State::open(&board::state_db(&self.repo)).map_err(|e| internal(&e))?,
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

#[derive(Serialize)]
struct StubNote<'a> {
    session: u64,
    rank: u64,
    seq: u64,
    files: Vec<(&'a str, Hash)>,
    summary: Option<&'a str>,
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::new(code::INVALID_PARAMS, e.to_string()))
}

fn reply<T: Serialize>(r: Result<T, RpcError>) -> Result<Value, RpcError> {
    r.map(|v| serde_json::to_value(v).expect("results always serialize"))
}
