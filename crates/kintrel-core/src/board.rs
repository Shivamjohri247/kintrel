//! A board on disk: `<repo>/.kintrel/` (SPEC §5.1).

use std::fs;
use std::path::{Path, PathBuf};

use crate::state::State;
use crate::{Error, Hash, Result, worktree};

pub fn data_dir(repo: &Path) -> PathBuf {
    repo.join(".kintrel")
}

/// Where every board's person key lives: `<cache>/kintrel-keys`, outside any board or view, so
/// a sandbox hides all of them without having to know which board an agent works on.
pub fn keys_dir() -> Result<PathBuf> {
    let cache = crate::os::cache_dir().ok_or_else(|| Error::Invalid("no cache directory: HOME (or LOCALAPPDATA) is not set".into()))?;
    Ok(cache.join("kintrel-keys"))
}

/// The person key of a board: approving a changed `kintrel.toml` needs it, and agents run
/// through `kintrel agent` (and checks) cannot read it. The daemon creates it.
pub fn person_key(repo: &Path) -> Result<PathBuf> {
    Ok(keys_dir()?.join(board_id(repo)?))
}

fn board_id(repo: &Path) -> Result<String> {
    Ok(Hash::of(fs::canonicalize(repo)?.as_os_str().as_encoded_bytes()).short())
}

/// What a person sends to approve head's config `theirs` over the promoted `trusted`: a hash
/// keyed by the person key and bound to that exact pair, so whoever relays it (a fake socket)
/// learns nothing that approves anything else.
pub fn approval(key: &str, trusted: Option<Hash>, theirs: Hash) -> Hash {
    let mut h = blake3::Hasher::new_keyed(blake3::hash(key.trim().as_bytes()).as_bytes());
    h.update(&trusted.unwrap_or_default().0);
    h.update(&theirs.0);
    Hash(*h.finalize().as_bytes())
}

pub fn state_db(repo: &Path) -> PathBuf {
    data_dir(repo).join("state.db")
}

pub fn log_db(repo: &Path) -> PathBuf {
    data_dir(repo).join("log.db")
}

/// Per-user runtime directory of a board: `<cache>/kintrel/<board id>`, the id being a hash of
/// the repo's canonical path. It holds the daemon socket and agent views. It is outside the repo,
/// so tools in a view never find the repo's `.git`, and short enough for a Unix socket path
/// (about 104 bytes on macOS) wherever the repo lives.
pub fn runtime_dir(repo: &Path) -> Result<PathBuf> {
    let cache = crate::os::cache_dir().ok_or_else(|| Error::Invalid("no cache directory: HOME (or LOCALAPPDATA) is not set".into()))?;
    Ok(cache.join("kintrel").join(board_id(repo)?))
}

/// Creates a board whose first head and promoted root is the current contents of `repo`.
pub fn init(repo: &Path) -> Result<Hash> {
    init_with(repo, |st| worktree::import_dir(st, repo))
}

/// Creates a board whose first head and promoted root is whatever `import` stores.
pub fn init_with(repo: &Path, import: impl FnOnce(&State) -> Result<Hash>) -> Result<Hash> {
    let dir = data_dir(repo);
    if state_db(repo).exists() {
        return Err(Error::Invalid(format!("{} is already a kintrel board", repo.display())));
    }
    fs::create_dir_all(&dir)?;
    crate::os::set_private_dir(&dir)?;
    crate::log::create(&log_db(repo))?;
    let mut state = State::open(&state_db(repo))?;
    state.batch(|s| {
        let root = import(s)?;
        s.set_refs(root, root, "init")?;
        Ok(root)
    })
}
