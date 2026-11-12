//! Daemon wire protocol (SPEC §12.1): JSON-RPC 2.0, one message per u32-BE length-prefixed frame.
//! Phase 0 covers `session.start`, `post`, `read.file` and `status`; the rest arrive with the engine.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Hash;

pub const SESSION_START: &str = "session.start";
pub const SESSION_REFRESH: &str = "session.refresh";
pub const POST: &str = "post";
pub const READ_FILE: &str = "read.file";
pub const STATUS: &str = "status";
pub const PROMOTE_NOW: &str = "promote.now";
pub const INBOX: &str = "inbox";
pub const INTENT: &str = "intent";
pub const CLAIM: &str = "claim";
pub const RELEASE: &str = "release";
pub const SESSION_HEARTBEAT: &str = "session.heartbeat";
pub const SESSION_END: &str = "session.end";
pub const REVERT: &str = "revert";
pub const CHECKS_RUN: &str = "checks.run";
pub const SYNC: &str = "git.sync";
/// Admin call: Prometheus text for `kintrel daemon metrics` (SPEC §15).
pub const METRICS: &str = "metrics";

/// Largest frame either side accepts.
pub const MAX_FRAME: u32 = 64 << 20;
/// Note limits (SPEC §4.3).
pub const MAX_BLOB: usize = 1 << 20;
pub const MAX_OPS: usize = 256;
/// Posts one session may make per second (SPEC §14); more are answered `BUSY`.
pub const MAX_POSTS_PER_SEC: u32 = 200;
/// Intent and claim summaries (SPEC §4.3).
pub const MAX_SUMMARY: usize = 4 << 10;

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("rpc error {code}: {message}")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

pub mod code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;
    pub const NOT_IMPLEMENTED: i64 = -32000;
    /// `data` is `{"retry_after_ms": n}`.
    pub const BUSY: i64 = -32001;
    pub const UNAUTHORIZED: i64 = -32002;
    pub const NOT_FOUND: i64 = -32003;
    /// The file changed since the caller read it. `data` is
    /// `{"path", "current_hash", "current"}`; `current` is null if the file is absent.
    pub const STALE_BASE: i64 = -32004;
    /// The change cannot be made cleanly; `data` is `{"atoms": [...]}`.
    pub const CONFLICT: i64 = -32005;
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        RpcError { code, message: message.into(), data: None }
    }

    pub fn busy(retry_after_ms: u64) -> Self {
        RpcError {
            data: Some(serde_json::json!({ "retry_after_ms": retry_after_ms })),
            ..Self::new(code::BUSY, "busy")
        }
    }

    pub fn retry_after_ms(&self) -> Option<u64> {
        if self.code != code::BUSY {
            return None;
        }
        self.data.as_ref()?.get("retry_after_ms")?.as_u64()
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionStart {
    pub agent: String,
    #[serde(default)]
    pub rank_hint: Option<u64>,
    #[serde(default)]
    pub base: Option<Hash>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionStarted {
    pub session: u64,
    pub token: String,
    pub rank: u64,
    pub base_root: Hash,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Refresh {
    pub token: String,
}

/// The session's view now starts at head. Its own notes are either in head or listed in
/// `lost` (conflicted: their changes are not in the new view).
#[derive(Debug, Serialize, Deserialize)]
pub struct Refreshed {
    pub base_root: Hash,
    pub lost: Vec<Hash>,
}

/// `git.sync`: bring `branch`'s (default: Git `HEAD`'s branch) commits since its sync mark, or
/// since the full commit id `since`, into the board.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Sync {
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub since: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Synced {
    pub branch: String,
    pub commits: Vec<SyncedCommit>,
}

/// One synced commit: its notes, and those that did not reach head (conflicted with agents'
/// work: `kintrel conflicts` shows them).
#[derive(Debug, Serialize, Deserialize)]
pub struct SyncedCommit {
    pub commit: String,
    pub summary: String,
    pub notes: Vec<Hash>,
    pub lost: Vec<Hash>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Post {
    pub token: String,
    pub files: Vec<FileWrite>,
    #[serde(default)]
    pub summary: Option<String>,
}

/// Whole new contents of one file; the daemon derives the atoms (SPEC §4.3). Text goes in
/// `content`; a file that is not UTF-8 (an image, say) goes in `binary`, hex-encoded. Neither
/// deletes the file.
#[derive(Debug, Serialize, Deserialize)]
pub struct FileWrite {
    pub path: String,
    pub content: Option<String>,
    /// If set, the post is rejected with `STALE_BASE` unless the file in the caller's view
    /// still has this hash (`Hash::default()` asserts the file is absent).
    #[serde(default)]
    pub base: Option<Hash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
}

impl FileWrite {
    /// A write of `bytes` (`None` deletes): as text if it is UTF-8, else hex-encoded.
    pub fn bytes(path: String, bytes: Option<Vec<u8>>, base: Option<Hash>) -> FileWrite {
        match bytes.map(String::from_utf8) {
            None => FileWrite { path, content: None, base, binary: None },
            Some(Ok(text)) => FileWrite { path, content: Some(text), base, binary: None },
            Some(Err(e)) => {
                let hex = e.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
                FileWrite { path, content: None, base, binary: Some(hex) }
            }
        }
    }

    /// The new contents (`None`: delete), decoded; `Err` names what is wrong.
    pub fn new_bytes(&self) -> Result<Option<Vec<u8>>, String> {
        match (&self.content, &self.binary) {
            (Some(_), Some(_)) => Err(format!("{:?}: set content or binary, not both", self.path)),
            (Some(text), None) => Ok(Some(text.clone().into_bytes())),
            (None, None) => Ok(None),
            (None, Some(hex)) => {
                let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
                let pairs = hex.as_bytes().chunks(2);
                let bytes: Option<Vec<u8>> = pairs.map(|p| Some(digit(*p.first()?)? << 4 | digit(*p.get(1)?)?)).collect();
                bytes.map(Some).ok_or_else(|| format!("{:?}: binary is not hex", self.path))
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Posted {
    pub note_id: Hash,
    pub gseq: u64,
    /// The write footprint as atom key text.
    pub atoms: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReadFile {
    pub token: String,
    pub path: String,
    #[serde(default)]
    pub at: Option<Hash>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileContent {
    pub path: String,
    pub content: String,
    pub hash: Hash,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Status {}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatusInfo {
    pub sessions: u64,
    pub notes: u64,
    pub head: Option<Hash>,
    pub promoted: Option<Hash>,
    /// Open conflicts after the latest combiner pass.
    #[serde(default)]
    pub conflicts: u64,
    /// Notes folded into head by the latest pass; `notes - folded` is the combiner lag.
    #[serde(default)]
    pub folded: u64,
    /// Notes in head not yet promoted, and how long promotion has waited for them (ms).
    #[serde(default)]
    pub unpromoted: u64,
    #[serde(default)]
    pub promotion_wait_ms: u64,
    /// Gate and feedback check runs queued or running, plus deploys waiting.
    #[serde(default)]
    pub ci_pending: u64,
    #[serde(default)]
    pub pass_ms: u64,
    /// Files in head that fail the gate and hold up promotion.
    #[serde(default)]
    pub blocked: Vec<String>,
    /// Gate checks failing on the latest promotion candidate.
    #[serde(default)]
    pub failing: Vec<String>,
}

/// Takes the session's unread notices.
#[derive(Debug, Serialize, Deserialize)]
pub struct Inbox {
    pub token: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Notices {
    pub notices: Vec<Notice>,
}

/// One notice (SPEC §8.3): small and machine-generated. Empty fields are left out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    /// `body_changed`, `interface_changed`, `deleted`, `conflict` or `many_changes`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub atom: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<NoticeBy>,
    /// Signatures; for `conflict`, the atom's text in base, in head (`kept`) and in yours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// For `intent` and `claim_overlap`: what the other agent said it is doing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theirs: Option<String>,
    /// Your atoms that use the changed one (dependents, SPEC §8.1 rule 3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub you_affected: Vec<String>,
    /// For `ci_failed`: the check's name and the tail of its output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// For `many_changes`: atoms whose notices the rate limit folded together.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub atoms: Vec<String>,
    pub action: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticeBy {
    pub agent: String,
    /// Short note id.
    pub note: String,
}

/// For requests that need only the session: `session.heartbeat`, `session.end`.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionRef {
    pub token: String,
}

/// `intent` or `claim` (SPEC §7.4). Targets are atom keys; a file-level key covers the file.
#[derive(Debug, Serialize, Deserialize)]
pub struct Announce {
    pub token: String,
    pub summary: String,
    pub targets: Vec<String>,
    /// Defaults to 10 minutes; activity from the session renews it.
    #[serde(default)]
    pub lease_ms: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Announced {
    pub id: u64,
    /// Live claims and intents of other sessions on the same atoms.
    pub overlapping: Vec<ClaimInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimInfo {
    pub id: u64,
    pub agent: String,
    /// `intent` or `claim`.
    pub kind: String,
    pub summary: String,
    pub targets: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Release {
    pub token: String,
    pub id: u64,
}

/// Undo one note's changes in your view, keeping everything since (SPEC §4.3 `Revert`). Posts
/// a note; fails with `CONFLICT` if the note's atoms changed since.
#[derive(Debug, Serialize, Deserialize)]
pub struct Revert {
    pub token: String,
    /// Full or abbreviated (8+ hex digits) note id.
    pub note: String,
}

/// Runs `kintrel.toml` checks (SPEC §10) on a session's view (`token`), a root (`at`), or head.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ChecksRun {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub at: Option<Hash>,
    /// `lint`, `typecheck` or `test`; all tiers if absent.
    #[serde(default)]
    pub tier: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ChecksRan {
    pub root: Hash,
    pub outcomes: Vec<CheckOutcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckOutcome {
    pub check: String,
    pub tier: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target: String,
    pub ok: bool,
    pub cached: bool,
    #[serde(default)]
    pub timed_out: bool,
    pub ms: u64,
    pub output: String,
    pub key: Hash,
}

/// `promote.now`. `approve_config` is the hash of the `kintrel.toml` blob a person reviewed:
/// head's changed config is approved for promotion only if it still has exactly that hash, and
/// `approval` is `board::approval` for it, made with the person key.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PromoteNow {
    #[serde(default)]
    pub approve_config: Option<Hash>,
    #[serde(default)]
    pub approval: Option<Hash>,
}
