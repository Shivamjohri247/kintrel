//! Materialized views (SPEC §5.4, §12.4): real directories that agents and their tools work in.
//!
//! A view lives in the board's runtime directory (`board::runtime_dir`), outside the repository,
//! so Git commands in it cannot touch the real repo: `<runtime>/views/<session>`. It is cloned
//! from a shared per-root checkout under `<runtime>/base/<root>` (APFS `clonefile`, else a copy). `.kintrel/view.json`
//! names the board and session; `.kintrel/manifest.json` records what the view held at its
//! last sync, so changes are found without asking the daemon.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use kintrel_core::state::State;
use kintrel_core::tree::Kind;
use kintrel_core::{Hash, board, worktree};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Marker {
    pub repo: PathBuf,
    pub session: u64,
    pub token: String,
    pub base_root: Hash,
}

/// What a file looked like at the last sync. Size and mtime let a scan skip unchanged files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub size: u64,
    pub mtime_ns: i64,
    pub hash: Hash,
}

pub type Manifest = BTreeMap<String, Stamp>;

/// One file whose view content differs from the manifest. `content: None` means deleted.
pub struct Change {
    pub path: String,
    pub content: Option<Vec<u8>>,
    /// The hash at the last sync, or `Hash::default()` for a new file (asserts it is absent).
    pub base: Hash,
}

fn open_store(repo: &Path) -> anyhow::Result<State> {
    Ok(State::open_with_log(&board::state_db(repo), &board::log_db(repo))?)
}

/// Creates (or recreates) the view for a session at `base_root` and returns its path.
pub fn materialize(marker: &Marker) -> anyhow::Result<PathBuf> {
    let st = open_store(&marker.repo)?;
    let cache = board::runtime_dir(&marker.repo)?;
    let base = worktree::base_checkout(&st, &marker.repo, marker.base_root)?;
    let view = cache.join("views").join(marker.session.to_string());
    if view.exists() {
        fs::remove_dir_all(&view)?;
    }
    fs::create_dir_all(view.parent().unwrap())?;
    worktree::clone_tree(&base, &view)?;
    let mut manifest = Manifest::new();
    for (path, _, hash) in worktree::files(&st, marker.base_root)? {
        manifest.insert(path.clone(), stamp(&view.join(&path), hash)?);
    }
    save(&view, marker, &manifest)?;
    Ok(view)
}

/// Moves an existing view to `marker.base_root` in place (the agent's working directory stays
/// valid). The caller posts local changes first; files that still differ are overwritten.
pub fn update(view: &Path, marker: &Marker) -> anyhow::Result<()> {
    let st = open_store(&marker.repo)?;
    let old = load_manifest(view)?;
    let mut manifest = Manifest::new();
    let new: BTreeMap<String, (Kind, Hash)> = worktree::files(&st, marker.base_root)?.into_iter().map(|(p, k, h)| (p, (k, h))).collect();
    // Deletions first, so a file that becomes a directory is out of the way.
    for path in old.keys().filter(|p| !new.contains_key(*p)) {
        if real_parents(view, path) {
            fs::remove_file(view.join(path)).ok();
            prune_empty_parents(view, path);
        }
    }
    for (path, &(kind, hash)) in &new {
        let file = view.join(path);
        if old.get(path).map(|s| s.hash) != Some(hash) {
            let bytes = st.get_blob(hash)?.with_context(|| format!("missing blob {hash}"))?;
            make_parents(view, path)?;
            match fs::symlink_metadata(&file) {
                Ok(m) if m.is_dir() => fs::remove_dir_all(&file)?,
                Ok(_) => fs::remove_file(&file)?,
                Err(_) => {}
            }
            match kind {
                Kind::Symlink => kintrel_core::os::write_symlink(&bytes, &file)?,
                _ => {
                    fs::write(&file, &bytes)?;
                    kintrel_core::os::set_executable(&file, kind == Kind::Exec)?;
                }
            }
        }
        manifest.insert(path.clone(), stamp(&file, hash)?);
    }
    save(view, marker, &manifest)
}

/// Finds the view containing `start` by walking up to its `.kintrel/view.json`.
pub fn find(start: &Path) -> anyhow::Result<(PathBuf, Marker)> {
    let start = fs::canonicalize(start).unwrap_or_else(|_| start.to_owned());
    for dir in start.ancestors() {
        let marker = dir.join(".kintrel/view.json");
        if marker.is_file() {
            let m: Marker = serde_json::from_slice(&fs::read(&marker)?).context("reading view marker")?;
            return Ok((dir.to_owned(), m));
        }
    }
    bail!("{} is not inside a kintrel view (create one with `kintrel view create`)", start.display())
}

/// Files that differ from the manifest: only `paths` (view-relative) if given, else a full scan.
pub fn changes(view: &Path, paths: Option<&[String]>) -> anyhow::Result<Vec<Change>> {
    let manifest = load_manifest(view)?;
    let candidates: Vec<String> = match paths {
        Some(p) => p.to_vec(),
        None => {
            let mut all = vec![];
            walk(view, &mut all)?;
            all.extend(manifest.keys().cloned());
            all.sort();
            all.dedup();
            all
        }
    };
    let mut out = vec![];
    for path in candidates {
        let file = view.join(&path);
        let old = manifest.get(&path);
        let meta = match fs::symlink_metadata(&file) {
            Ok(m) if m.is_file() => m,
            Ok(_) => continue, // directories and symlinks are not posted from views yet
            Err(_) => {
                if let Some(old) = old {
                    out.push(Change { path, content: None, base: old.hash });
                }
                continue;
            }
        };
        if old.is_some_and(|s| s.size == meta.len() && s.mtime_ns == mtime_ns(&meta)) {
            continue;
        }
        let bytes = fs::read(&file)?;
        if old.is_some_and(|s| s.hash == Hash::of(&bytes)) {
            continue;
        }
        out.push(Change { path, content: Some(bytes), base: old.map_or(Hash::default(), |s| s.hash) });
    }
    Ok(out)
}

/// Records that `posted` changes are now part of the session's view.
pub fn mark_posted(view: &Path, posted: &[Change]) -> anyhow::Result<()> {
    let (_, marker) = find(view)?;
    let mut manifest = load_manifest(view)?;
    for c in posted {
        match &c.content {
            Some(bytes) => {
                manifest.insert(c.path.clone(), stamp(&view.join(&c.path), Hash::of(bytes))?);
            }
            None => {
                manifest.remove(&c.path);
            }
        }
    }
    save(view, &marker, &manifest)
}

/// A path given on the command line, relative to the view root.
pub fn rel_path(view: &Path, arg: &Path) -> anyhow::Result<String> {
    let abs = if arg.is_absolute() { arg.to_owned() } else { std::env::current_dir()?.join(arg) };
    // The file may be deleted, so canonicalize its parent only.
    let parent = abs.parent().context("bad path")?;
    let parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_owned());
    let full = parent.join(abs.file_name().context("bad path")?);
    let rel = full.strip_prefix(view).with_context(|| format!("{} is outside the view {}", full.display(), view.display()))?;
    rel.to_str().map(str::to_owned).context("non-UTF-8 path")
}

/// Untracked files in the view, minus what `.gitignore` (and `.ignore`, and the user's global
/// Git excludes) leave out, as Git would: build output and caches never reach the board.
/// Tracked files come from the manifest, so ignoring never hides an edit to one.
fn walk(view: &Path, out: &mut Vec<String>) -> anyhow::Result<()> {
    let walker = ignore::WalkBuilder::new(view)
        .hidden(false)
        .parents(false) // ignore files above the view belong to someone else's tree
        .require_git(false)
        .filter_entry(|e| e.depth() == 0 || e.file_name().to_str().is_some_and(|n| n != ".kintrel" && n != ".git"))
        .build();
    for item in walker {
        let item = item?;
        if !item.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Some(path) = item.path().strip_prefix(view)?.to_str() else { continue };
        if kintrel_core::path::is_safe_rel_path(path) {
            out.push(path.to_owned());
        }
    }
    Ok(())
}

/// The agent owns the view and may plant symlinks in it. Writes from the store must never follow
/// one out of the view: every parent of `path` becomes a real directory. A link or file in the
/// way is removed (the link itself, never its target).
fn make_parents(view: &Path, path: &str) -> anyhow::Result<()> {
    let mut dir = view.to_owned();
    let parents: Vec<&str> = path.split('/').collect();
    for seg in &parents[..parents.len() - 1] {
        dir.push(seg);
        match fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() => continue,
            Ok(_) => fs::remove_file(&dir)?,
            Err(_) => {}
        }
        fs::create_dir(&dir)?;
    }
    Ok(())
}

/// Every parent of `path` inside the view is a real directory (no symlinks).
fn real_parents(view: &Path, path: &str) -> bool {
    let mut dir = view.to_owned();
    let parents: Vec<&str> = path.split('/').collect();
    parents[..parents.len() - 1].iter().all(|seg| {
        dir.push(seg);
        fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir())
    })
}

fn prune_empty_parents(view: &Path, path: &str) {
    let mut dir = Path::new(path).parent();
    while let Some(d) = dir.filter(|d| !d.as_os_str().is_empty()) {
        if fs::remove_dir(view.join(d)).is_err() {
            break; // not empty
        }
        dir = d.parent();
    }
}

fn mtime_ns(m: &fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as i64)
}

fn stamp(file: &Path, hash: Hash) -> anyhow::Result<Stamp> {
    let m = fs::symlink_metadata(file)?;
    Ok(Stamp { size: m.len(), mtime_ns: mtime_ns(&m), hash })
}

/// The manifest sits in the agent-writable view, so its keys are untrusted: anything that is not
/// a safe relative path (no `..`, no absolute paths) is dropped.
fn load_manifest(view: &Path) -> anyhow::Result<Manifest> {
    let mut m: Manifest = serde_json::from_slice(&fs::read(view.join(".kintrel/manifest.json"))?)?;
    m.retain(|path, _| kintrel_core::path::is_safe_rel_path(path));
    Ok(m)
}

fn save(view: &Path, marker: &Marker, manifest: &Manifest) -> anyhow::Result<()> {
    let dir = view.join(".kintrel");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("view.json"), serde_json::to_vec_pretty(marker)?)?;
    let tmp = dir.join("manifest.json.tmp");
    fs::write(&tmp, serde_json::to_vec(manifest)?)?;
    fs::rename(tmp, dir.join("manifest.json"))?;
    Ok(())
}
