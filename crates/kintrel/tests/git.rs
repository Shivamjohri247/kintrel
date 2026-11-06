//! Git round trip (SPEC §11, B8): import HEAD, export, get the identical tree back.

use std::fs;
use std::path::Path;
use std::process::Command;

use kintrel_core::board;
use kintrel_core::state::{self, State};

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .expect("git is installed");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn import_then_export_gives_the_same_tree() {
    let repo = std::env::temp_dir().join(format!("kt-git-{}", std::process::id()));
    fs::remove_dir_all(&repo).ok();
    fs::create_dir_all(repo.join("a")).unwrap();
    fs::write(repo.join("text.txt"), "hello\n").unwrap();
    fs::write(repo.join("empty"), "").unwrap();
    fs::write(repo.join("run.sh"), "#!/bin/sh\n").unwrap();
    // An executable and a symlink exist only on Unix (see kintrel_core::os).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(repo.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("text.txt", repo.join("link")).unwrap();
    }
    fs::write(repo.join("bin.dat"), (0..=255u8).collect::<Vec<_>>()).unwrap();
    fs::write(repo.join("latin1.txt"), b"caf\xe9\n").unwrap();
    fs::write(repo.join("a/x"), "in a dir\n").unwrap();
    fs::write(repo.join("a.txt"), "beside the dir\n").unwrap();
    fs::write(repo.join("a-b"), "sorts before both\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "corpus"]);

    let root = board::init_with(&repo, |st| {
        kintrel::git::import_head(st, &repo).map_err(|e| kintrel_core::Error::Invalid(e.to_string()))
    })
    .unwrap();
    let st = State::open_with_log(&board::state_db(&repo), &board::log_db(&repo)).unwrap();
    assert_eq!(st.get_ref(state::PROMOTED).unwrap(), Some(root));

    let commit = kintrel::git::export(&st, &repo, root, "kintrel/main").unwrap();
    assert!(commit.is_none(), "the branch starts at HEAD, whose tree is already this root");

    // Change the root, export again: a new commit whose parent is the imported HEAD.
    let changed = st
        .apply(root, &[("text.txt".to_owned(), Some((kintrel_core::tree::Kind::File, st.put_blob(b"bye\n").unwrap())))].into())
        .unwrap();
    let commit = kintrel::git::export(&st, &repo, changed, "kintrel/main").unwrap().unwrap();
    assert_eq!(git(&repo, &["rev-parse", &format!("{commit}^")]), git(&repo, &["rev-parse", "HEAD"]));
    assert_eq!(git(&repo, &["show", "kintrel/main:text.txt"]), "bye");
    // Everything but text.txt is byte-identical to HEAD: same blob ids.
    let ls = |r: &str| git(&repo, &["ls-tree", "-r", r]).lines().filter(|l| !l.ends_with("text.txt")).map(str::to_owned).collect::<Vec<_>>();
    assert_eq!(ls("kintrel/main"), ls("HEAD"));
    fs::remove_dir_all(&repo).ok();
}
