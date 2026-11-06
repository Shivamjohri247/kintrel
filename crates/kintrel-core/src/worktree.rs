//! Moving trees between the store and a real directory.

use std::fs;
use std::path::Path;

use crate::path::is_safe_rel_path;
use crate::state::State;
use crate::tree::{Entry, Kind, Tree};
use crate::{Error, Hash, Result};

/// Imports `dir` into the store and returns its root. Skips anything an agent could not post
/// (`.git` anywhere, top-level `.kintrel`), special files and empty directories (as Git does).
// ponytail: no ignore rules; plain `init` takes everything. Repos use `init --from-git` (HEAD tree).
pub fn import_dir(state: &State, dir: &Path) -> Result<Hash> {
    match import(state, dir, "")? {
        Some(root) => Ok(root),
        None => state.put_tree(&Tree::default()),
    }
}

fn import(state: &State, dir: &Path, rel: &str) -> Result<Option<Hash>> {
    let mut entries = Vec::new();
    for item in fs::read_dir(dir)? {
        let item = item?;
        let name = item.file_name().into_string().map_err(|n| {
            Error::Invalid(format!("non-UTF-8 file name {:?} in {}", n, dir.display()))
        })?;
        let child = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        if !is_safe_rel_path(&child) {
            continue;
        }
        let path = item.path();
        let meta = fs::symlink_metadata(&path)?;
        let ft = meta.file_type();
        let (kind, id) = if ft.is_symlink() {
            (Kind::Symlink, state.put_blob(&crate::os::read_symlink(&path)?)?)
        } else if ft.is_file() {
            let kind = if crate::os::is_executable(&meta) { Kind::Exec } else { Kind::File };
            (kind, state.put_blob(&fs::read(&path)?)?)
        } else if ft.is_dir() {
            match import(state, &path, &child)? {
                Some(id) => (Kind::Dir, id),
                None => continue,
            }
        } else {
            continue;
        };
        entries.push(Entry { name, kind, id });
    }
    if entries.is_empty() {
        return Ok(None);
    }
    state.put_tree(&Tree::new(entries)?).map(Some)
}

/// Writes the tree `root` into `dest`, which must not exist yet.
pub fn checkout(state: &State, root: Hash, dest: &Path) -> Result<()> {
    fs::create_dir(dest)?;
    for e in state.get_tree(root)?.entries() {
        let path = dest.join(&e.name);
        let blob = || state.get_blob(e.id)?.ok_or_else(|| Error::Corrupt(format!("missing blob {}", e.id)));
        match e.kind {
            Kind::Dir => checkout(state, e.id, &path)?,
            Kind::Symlink => crate::os::write_symlink(&blob()?, &path)?,
            Kind::File | Kind::Exec => {
                fs::write(&path, blob()?)?;
                crate::os::set_executable(&path, e.kind == Kind::Exec)?;
            }
        }
    }
    Ok(())
}

/// Every file under `root` as (path, kind, blob), in tree order.
pub fn files(st: &State, root: Hash) -> Result<Vec<(String, Kind, Hash)>> {
    fn walk(st: &State, id: Hash, prefix: &str, out: &mut Vec<(String, Kind, Hash)>) -> Result<()> {
        for e in st.get_tree(id)?.entries() {
            let path = if prefix.is_empty() { e.name.clone() } else { format!("{prefix}/{}", e.name) };
            match e.kind {
                Kind::Dir => walk(st, e.id, &path, out)?,
                kind => out.push((path, kind, e.id)),
            }
        }
        Ok(())
    }
    let mut out = vec![];
    walk(st, root, "", &mut out)?;
    Ok(out)
}

/// The shared, read-only checkout of `root` under `<runtime>/base/<root>`, created on first use.
/// Views and check runs clone it ([`clone_tree`]) rather than write into it.
pub fn base_checkout(st: &State, repo: &Path, root: Hash) -> Result<std::path::PathBuf> {
    let dir = crate::board::runtime_dir(repo)?.join("base");
    let base = dir.join(root.to_string());
    if !base.exists() {
        // Check out beside the target and rename, so a half-written base is never used.
        let tmp = dir.join(format!("{root}.tmp-{}", std::process::id()));
        fs::create_dir_all(&dir)?;
        fs::remove_dir_all(&tmp).ok();
        checkout(st, root, &tmp)?;
        if fs::rename(&tmp, &base).is_err() && !base.exists() {
            return Err(Error::Invalid(format!("could not create base checkout {}", base.display())));
        }
        fs::remove_dir_all(&tmp).ok();
    }
    Ok(base)
}

/// Copies a directory tree, sharing file blocks where the filesystem can (APFS `clonefile`).
/// Symlinks are copied as links.
pub fn clone_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir(dst)?;
    for item in fs::read_dir(src)? {
        let item = item?;
        let (from, to) = (item.path(), dst.join(item.file_name()));
        let ft = item.file_type()?;
        if ft.is_dir() {
            clone_tree(&from, &to)?;
        } else if ft.is_symlink() {
            crate::os::write_symlink(&crate::os::read_symlink(&from)?, &to)?;
        } else {
            reflink_copy::reflink_or_copy(&from, &to)?;
        }
    }
    Ok(())
}

// Modes and symlinks round-trip exactly only on Unix (see os.rs).
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn import_then_checkout_is_lossless() {
        let base = std::env::temp_dir().join(format!("kintrel-wt-test-{}", std::process::id()));
        fs::remove_dir_all(&base).ok();
        let src = base.join("src");
        fs::create_dir_all(src.join("nested/deep")).unwrap();
        fs::create_dir_all(src.join("empty_dir")).unwrap();
        fs::create_dir_all(src.join(".git")).unwrap();
        fs::write(src.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(src.join("text.txt"), "hello\n").unwrap();
        fs::write(src.join("empty"), "").unwrap();
        fs::write(src.join("bin.dat"), (0..=255u8).collect::<Vec<_>>()).unwrap();
        fs::write(src.join("latin1.txt"), b"caf\xe9\n").unwrap();
        fs::write(src.join("run.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(src.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(src.join("nested/deep/lib.rs"), "pub fn f() {}\n").unwrap();
        symlink("text.txt", src.join("link")).unwrap();

        let mut state = State::open(&base.join("state.db")).unwrap();
        let root = state.batch(|s| import_dir(s, &src)).unwrap();
        let out = base.join("out");
        checkout(&state, root, &out).unwrap();

        // Re-importing the checkout gives the same root: contents, modes and links all survived.
        assert_eq!(state.batch(|s| import_dir(s, &out)).unwrap(), root);
        assert_eq!(fs::read(out.join("latin1.txt")).unwrap(), b"caf\xe9\n");
        assert_eq!(fs::read_link(out.join("link")).unwrap(), Path::new("text.txt"));
        assert!(!out.join(".git").exists() && !out.join("empty_dir").exists());

        let (kind, id) = state.lookup(root, "nested/deep/lib.rs").unwrap().unwrap();
        assert_eq!((kind, state.get_blob(id).unwrap().unwrap()), (Kind::File, b"pub fn f() {}\n".to_vec()));
        assert_eq!(state.lookup(root, "run.sh").unwrap().unwrap().0, Kind::Exec);
        assert!(state.lookup(root, "text.txt/x").unwrap().is_none());
        assert!(state.lookup(root, "missing").unwrap().is_none());
        fs::remove_dir_all(&base).ok();
    }
}
