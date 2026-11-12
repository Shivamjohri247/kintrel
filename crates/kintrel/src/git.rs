//! Git interop (SPEC §11): import a Git `HEAD` tree as the first root, export promoted roots as
//! commits. Modes map one to one: 100644 File, 100755 Exec, 120000 Symlink, 040000 Dir.

use std::path::Path;

use anyhow::{Context, bail};
use gix::objs::tree::{Entry as GitEntry, EntryKind};
use kintrel_core::state::State;
use kintrel_core::tree::{Entry, Kind, Tree};
use kintrel_core::{Hash, path::is_safe_rel_path};

pub const DEFAULT_BRANCH: &str = "kintrel/main";
/// The author of notes `kintrel sync` makes; reserved, so no client session can take it.
pub const SYNC_AGENT: &str = "git";

/// Opens a repository. Where Git has no identity configured (a fresh machine, a container),
/// updating a ref's reflog would fail, so Kintrel's own identity is the fallback; a configured
/// `user.name`/`user.email` always wins.
fn open(git_dir: &Path) -> anyhow::Result<gix::Repository> {
    use gix::config::tree::gitoxide::Committer;
    let mut repo = gix::open(git_dir).with_context(|| format!("opening Git repository {}", git_dir.display()))?;
    let mut config = repo.config_snapshot_mut();
    config.set_value(&Committer::NAME_FALLBACK, "Kintrel")?;
    config.set_value(&Committer::EMAIL_FALLBACK, "kintrel@localhost")?;
    config.commit()?;
    Ok(repo)
}

/// Imports the tree of `HEAD` in the Git repository at `git_dir` and returns its root. Also
/// starts [`DEFAULT_BRANCH`] at that commit (if the branch is new), so the first export's
/// parent is the imported commit and the branch merges into the repo's history.
/// Submodules (gitlinks) are skipped with a warning.
// ponytail: submodules and LFS pointers as opaque entries (SPEC §11) when someone needs them.
pub fn import_head(st: &State, git_dir: &Path) -> anyhow::Result<Hash> {
    let repo = open(git_dir)?;
    let head = repo.head_commit().context("reading HEAD")?;
    let tree = head.tree_id().context("HEAD has no tree")?;
    // `kintrel sync` brings in this branch's later commits.
    if let Some(name) = repo.head_name()? {
        let mark = sync_mark(&name.shorten().to_string());
        repo.reference(mark.as_str(), head.id, gix::refs::transaction::PreviousValue::Any, "kintrel: import")?;
    }
    let branch = format!("refs/heads/{DEFAULT_BRANCH}");
    if repo.try_find_reference(branch.as_str())?.is_none() {
        repo.reference(
            branch.as_str(),
            head.id,
            gix::refs::transaction::PreviousValue::MustNotExist,
            "kintrel: import",
        )?;
    }
    match import_tree(st, &repo, tree.detach(), "")? {
        Some(root) => Ok(root),
        None => Ok(st.put_tree(&Tree::default())?),
    }
}

/// Imports the tree of any commit (`rev`: a full commit id) and returns its root. Unlike
/// [`import_head`] it touches no refs (for replaying history, B7).
pub fn import_rev(st: &State, git_dir: &Path, rev: &str) -> anyhow::Result<Hash> {
    let repo = open(git_dir)?;
    let id = gix::ObjectId::from_hex(rev.as_bytes()).with_context(|| format!("{rev:?} is not a full commit id"))?;
    let commit = repo.find_object(id)?.try_into_commit()?;
    match import_tree(st, &repo, commit.tree_id()?.detach(), "")? {
        Some(root) => Ok(root),
        None => Ok(st.put_tree(&Tree::default())?),
    }
}

fn import_tree(st: &State, repo: &gix::Repository, id: gix::ObjectId, prefix: &str) -> anyhow::Result<Option<Hash>> {
    let object = repo.find_object(id)?;
    let tree = object.try_into_tree()?;
    let mut entries = vec![];
    for e in tree.decode()?.entries.iter() {
        let name = std::str::from_utf8(e.filename).with_context(|| format!("non-UTF-8 file name in {prefix:?}"))?;
        let path = if prefix.is_empty() { name.to_owned() } else { format!("{prefix}/{name}") };
        if !is_safe_rel_path(&path) {
            continue;
        }
        let (kind, child) = match e.mode.kind() {
            EntryKind::Tree => match import_tree(st, repo, e.oid.to_owned(), &path)? {
                Some(h) => (Kind::Dir, h),
                None => continue,
            },
            EntryKind::Commit => {
                eprintln!("kintrel: skipping submodule {path}");
                continue;
            }
            kind => {
                let blob = repo.find_object(e.oid)?;
                let k = match kind {
                    EntryKind::BlobExecutable => Kind::Exec,
                    EntryKind::Link => Kind::Symlink,
                    _ => Kind::File,
                };
                (k, st.put_blob(&blob.data)?)
            }
        };
        entries.push(Entry { name: name.to_owned(), kind, id: child });
    }
    if entries.is_empty() {
        return Ok(None);
    }
    Ok(Some(st.put_tree(&Tree::new(entries)?)?))
}

/// Writes `root` to the Git repository at `git_dir` and commits it on `branch` (parent: the
/// branch's current tip). Returns the new commit, or `None` if the tip already has this tree.
/// For the promoted root, the message names every note promoted since the parent's export
/// (`Kintrel-Note: <id> <author>` trailers; notes `kintrel sync` brought from Git are left out).
pub fn export(st: &State, git_dir: &Path, root: Hash, branch: &str) -> anyhow::Result<Option<gix::ObjectId>> {
    let repo = open(git_dir)?;
    let tree = export_tree(st, &repo, root)?;
    let refname = format!("refs/heads/{branch}");
    let parent = match repo.try_find_reference(refname.as_str())? {
        Some(mut r) => Some(r.peel_to_commit()?),
        None => None,
    };
    if let Some(p) = &parent
        && p.tree_id()?.detach() == tree
    {
        return Ok(None);
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
    let sig = gix::actor::Signature {
        name: "Kintrel".into(),
        email: "kintrel@localhost".into(),
        time: gix::date::Time { seconds: now, offset: 0 },
    };
    let mut time = gix::date::parse::TimeBuf::default();
    let sig = sig.to_ref(&mut time);
    let mut message = format!("kintrel: promoted root {}\n\nKintrel-Root: {root}\n", root.short());
    if st.get_ref(kintrel_core::state::PROMOTED)? == Some(root) {
        let upto = st.get_mark(kintrel_core::state::PROMOTED_GSEQ)?;
        let after = parent
            .as_ref()
            .and_then(|p| p.message_raw().ok().map(|m| m.to_string()))
            .and_then(|m| m.lines().find_map(|l| l.strip_prefix("Kintrel-Gseq: ").and_then(|n| n.trim().parse().ok())))
            .unwrap_or(0);
        message.push_str(&format!("Kintrel-Gseq: {upto}\n"));
        let notes: Vec<_> = st.promoted_between(after, upto)?.into_iter().filter(|(_, author)| author != SYNC_AGENT).collect();
        // Capped so a long-unexported board stays a readable message; `kintrel log` lists all.
        const SHOWN: usize = 200;
        for (id, author) in notes.iter().take(SHOWN) {
            // Names are checked at session start; control characters are dropped all the same.
            let author: String = author.chars().filter(|c| !c.is_control()).collect();
            message.push_str(&format!("Kintrel-Note: {id} {author}\n"));
        }
        if notes.len() > SHOWN {
            message.push_str(&format!("Kintrel-Notes-Omitted: {}\n", notes.len() - SHOWN));
        }
    }
    let parents: Vec<gix::ObjectId> = parent.iter().map(|p| p.id).collect();
    let id = repo.commit_as(sig, sig, refname.as_str(), message, tree, parents)?;
    Ok(Some(id.detach()))
}

fn export_tree(st: &State, repo: &gix::Repository, id: Hash) -> anyhow::Result<gix::ObjectId> {
    let tree = st.get_tree(id)?;
    let mut entries = Vec::with_capacity(tree.entries().len());
    for e in tree.entries() {
        let (mode, oid) = match e.kind {
            Kind::Dir => (EntryKind::Tree, export_tree(st, repo, e.id)?),
            kind => {
                let bytes = st.get_blob(e.id)?.with_context(|| format!("missing blob {}", e.id))?;
                let mode = match kind {
                    Kind::Exec => EntryKind::BlobExecutable,
                    Kind::Symlink => EntryKind::Link,
                    _ => EntryKind::Blob,
                };
                (mode, repo.write_blob(&bytes)?.detach())
            }
        };
        entries.push(GitEntry { mode: mode.into(), filename: e.name.as_str().into(), oid });
    }
    // Git's order: names compared as if directories had a trailing '/'.
    entries.sort();
    if entries.windows(2).any(|w| w[0].filename == w[1].filename) {
        bail!("duplicate entry in tree {id}");
    }
    Ok(repo.write_object(gix::objs::Tree { entries })?.detach())
}

/// The ref that records the last commit of `branch` brought into the board by `kintrel sync`.
pub fn sync_mark(branch: &str) -> String {
    format!("refs/kintrel/synced/{branch}")
}

/// One Git commit to bring into the board: its files that differ from its base, with the base's
/// version (`None`: absent) and the commit's (`None`: deleted).
pub struct Commit {
    pub id: gix::ObjectId,
    pub summary: String,
    pub files: Vec<FileChange>,
}

pub struct FileChange {
    pub path: String,
    pub base: Option<(Kind, Vec<u8>)>,
    pub new: Option<(Kind, Vec<u8>)>,
}

/// Commits on `branch` since its sync mark (or `since`, a full commit id), oldest first, following first parents.
/// Kintrel's own exports (on `kintrel/main`) are skipped: the board has them already.
/// A merge commit is diffed against the parent Kintrel exported, if one is, else its first
/// parent: merging `kintrel/main` into the branch brings in only the person's side.
pub fn pending(git_dir: &Path, branch: &str, since: Option<&str>) -> anyhow::Result<Vec<Commit>> {
    let repo = open(git_dir)?;
    let mark = match since {
        Some(rev) => gix::ObjectId::from_hex(rev.as_bytes()).with_context(|| format!("{rev:?} is not a full commit id"))?,
        None => repo
            .try_find_reference(sync_mark(branch).as_str())?
            .with_context(|| format!("{branch} has never been synced: give the commit the board started from with --since"))?
            .peel_to_commit()?
            .id,
    };
    let tip = repo.find_reference(format!("refs/heads/{branch}").as_str())?.peel_to_commit()?;
    let mut chain = vec![];
    let mut at = tip;
    // ponytail: walks first parents back to the mark, diffing whole trees per commit; fine for
    // the commits people make between syncs.
    while at.id != mark {
        let first = at.parent_ids().next().with_context(|| format!("{} is not an ancestor of {branch} (was it rewritten?)", mark))?;
        let next = repo.find_object(first)?.try_into_commit()?;
        chain.push(at);
        at = next;
    }
    // Kintrel's exports: the first-parent line of `kintrel/main` back to the commit it started
    // from, each with a `Kintrel-Root:` trailer. Only Kintrel writes that branch; a person's
    // commit that merely mentions the trailer is not an export.
    let trailer = |c: &gix::Commit| c.message_raw().is_ok_and(|m| m.to_string().contains("\nKintrel-Root: "));
    let mut exports = std::collections::HashSet::new();
    if let Some(mut r) = repo.try_find_reference(format!("refs/heads/{DEFAULT_BRANCH}").as_str())? {
        let mut at = Some(r.peel_to_commit()?);
        while let Some(c) = at.take().filter(trailer) {
            exports.insert(c.id);
            if let Some(p) = c.parent_ids().next() {
                at = Some(repo.find_object(p)?.try_into_commit()?);
            }
        }
    }
    let exported = |c: &gix::Commit| exports.contains(&c.id);
    let mut out = vec![];
    for c in chain.into_iter().rev() {
        if exported(&c) {
            continue;
        }
        let parents: Vec<gix::Commit> = c.parent_ids().map(|p| Ok(repo.find_object(p)?.try_into_commit()?)).collect::<anyhow::Result<_>>()?;
        let base = parents.iter().find(|p| exported(p)).or(parents.first());
        let before = match base {
            Some(b) => files(&repo, b.tree_id()?.detach(), "")?,
            None => Default::default(),
        };
        let after = files(&repo, c.tree_id()?.detach(), "")?;
        let blob = |e: Option<&(Kind, gix::ObjectId)>| -> anyhow::Result<Option<(Kind, Vec<u8>)>> {
            e.map(|(k, id)| Ok((*k, repo.find_object(*id)?.data.clone()))).transpose()
        };
        let mut changes = vec![];
        for path in before.keys().chain(after.keys()).collect::<std::collections::BTreeSet<_>>() {
            let (b, a) = (before.get(path), after.get(path));
            if b != a {
                changes.push(FileChange { path: path.clone(), base: blob(b)?, new: blob(a)? });
            }
        }
        let summary = c.message()?.summary().to_string();
        out.push(Commit { id: c.id, summary, files: changes });
    }
    Ok(out)
}

/// Every file under a tree, by path, as Kintrel imports them (submodules and unsafe paths skipped).
fn files(repo: &gix::Repository, id: gix::ObjectId, prefix: &str) -> anyhow::Result<std::collections::BTreeMap<String, (Kind, gix::ObjectId)>> {
    let mut out = std::collections::BTreeMap::new();
    let object = repo.find_object(id)?;
    for e in object.try_into_tree()?.decode()?.entries.iter() {
        let name = std::str::from_utf8(e.filename).with_context(|| format!("non-UTF-8 file name in {prefix:?}"))?;
        let path = if prefix.is_empty() { name.to_owned() } else { format!("{prefix}/{name}") };
        if !is_safe_rel_path(&path) {
            continue;
        }
        let kind = match e.mode.kind() {
            EntryKind::Tree => {
                out.extend(files(repo, e.oid.to_owned(), &path)?);
                continue;
            }
            EntryKind::Commit => continue,
            EntryKind::BlobExecutable => Kind::Exec,
            EntryKind::Link => Kind::Symlink,
            _ => Kind::File,
        };
        out.insert(path, (kind, e.oid.to_owned()));
    }
    Ok(out)
}

/// The branch Git's `HEAD` is on.
pub fn head_branch(git_dir: &Path) -> anyhow::Result<String> {
    let repo = open(git_dir)?;
    let name = repo.head_name()?.context("Git HEAD is detached: name the branch with --branch")?;
    Ok(name.shorten().to_string())
}

/// Moves `branch`'s sync mark to `commit`.
pub fn set_sync_mark(git_dir: &Path, branch: &str, commit: gix::ObjectId) -> anyhow::Result<()> {
    let repo = open(git_dir)?;
    repo.reference(sync_mark(branch).as_str(), commit, gix::refs::transaction::PreviousValue::Any, "kintrel: sync")?;
    Ok(())
}
