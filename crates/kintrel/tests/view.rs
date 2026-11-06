//! A view is agent-writable, so refreshing it must not trust what the agent left there.
//! The attacks plant Unix symlinks; on Windows that needs a privilege agents rarely have.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;

use kintrel::view::{self, Marker};
use kintrel_core::state::State;
use kintrel_core::tree::Kind;
use kintrel_core::{Hash, board};

#[test]
fn refresh_stays_inside_a_tampered_view() {
    let base = std::env::temp_dir().join(format!("kt-view-{}", std::process::id()));
    fs::remove_dir_all(&base).ok();
    let repo = base.join("repo");
    let outside = base.join("outside");
    fs::create_dir_all(repo.join("d")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    fs::write(repo.join("d/b.txt"), "b\n").unwrap();
    fs::write(base.join("escape.txt"), "must survive\n").unwrap();
    let r1 = board::init(&repo).unwrap();

    // Root 2: a.txt deleted, d/b.txt changed, d/c.txt added.
    let st = State::open(&board::state_db(&repo)).unwrap();
    let mut changes = BTreeMap::new();
    changes.insert("a.txt".to_owned(), None);
    changes.insert("d/b.txt".to_owned(), Some((Kind::File, st.put_blob(b"b2\n").unwrap())));
    changes.insert("d/c.txt".to_owned(), Some((Kind::File, st.put_blob(b"c\n").unwrap())));
    let r2 = st.apply(r1, &changes).unwrap();

    let mut marker = Marker { repo: fs::canonicalize(&repo).unwrap(), session: 1, token: String::new(), base_root: r1 };
    let v = view::materialize(&marker).unwrap();
    assert_eq!(fs::read_to_string(v.join("d/b.txt")).unwrap(), "b\n");

    // The agent plants a manifest key that climbs out, and swaps d/ for a link to outside/.
    let mpath = v.join(".kintrel/manifest.json");
    let mut m: serde_json::Value = serde_json::from_slice(&fs::read(&mpath).unwrap()).unwrap();
    let escape = format!("{}{}", "../".repeat(20), base.join("escape.txt").strip_prefix("/").unwrap().display());
    m[&escape] = serde_json::json!({ "size": 1, "mtime_ns": 0, "hash": Hash::default() });
    fs::write(&mpath, serde_json::to_vec(&m).unwrap()).unwrap();
    fs::remove_dir_all(v.join("d")).unwrap();
    symlink(&outside, v.join("d")).unwrap();

    marker.base_root = r2;
    view::update(&v, &marker).unwrap();

    assert_eq!(fs::read_to_string(base.join("escape.txt")).unwrap(), "must survive\n");
    assert!(fs::read_dir(&outside).unwrap().next().is_none(), "nothing written through the link");
    assert!(fs::symlink_metadata(v.join("d")).unwrap().is_dir());
    assert_eq!(fs::read_to_string(v.join("d/b.txt")).unwrap(), "b2\n");
    assert_eq!(fs::read_to_string(v.join("d/c.txt")).unwrap(), "c\n");
    assert!(!v.join("a.txt").exists());

    fs::remove_dir_all(board::runtime_dir(&repo).unwrap()).ok();
    fs::remove_dir_all(&base).ok();
}

#[test]
fn scan_skips_gitignored_files_but_not_tracked_ones() {
    let repo = std::env::temp_dir().join(format!("kt-view-ignore-{}", std::process::id()));
    fs::remove_dir_all(&repo).ok();
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join(".gitignore"), "build/\n*.log\n").unwrap();
    fs::write(repo.join("kept.log"), "tracked though ignored\n").unwrap();
    let root = board::init(&repo).unwrap();
    let marker = Marker { repo: fs::canonicalize(&repo).unwrap(), session: 1, token: String::new(), base_root: root };
    let v = view::materialize(&marker).unwrap();

    fs::create_dir_all(v.join("build")).unwrap();
    fs::write(v.join("build/out.txt"), "generated\n").unwrap();
    fs::write(v.join("debug.log"), "noise\n").unwrap();
    fs::write(v.join("new.txt"), "real work\n").unwrap();
    fs::write(v.join("kept.log"), "edited\n").unwrap();

    let mut got: Vec<String> = view::changes(&v, None).unwrap().into_iter().map(|c| c.path).collect();
    got.sort();
    assert_eq!(got, ["kept.log", "new.txt"]);

    fs::remove_dir_all(board::runtime_dir(&repo).unwrap()).ok();
    fs::remove_dir_all(&repo).ok();
}
