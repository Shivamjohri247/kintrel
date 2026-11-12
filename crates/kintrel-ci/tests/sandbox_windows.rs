//! The Windows check sandbox (an AppContainer per run), as `run.rs::sandbox_confines_checks`
//! checks the macOS and Linux ones: a check writes its checkout, and nothing else of the user's.
#![cfg(windows)]

use std::fs;

use kintrel_ci::{Ci, Snapshot, config_of};
use kintrel_core::board;
use kintrel_core::state::State;

#[test]
fn sandbox_confines_checks() {
    let repo = std::env::temp_dir().join(format!("kt-ci-sbx-win-{}", std::process::id()));
    fs::remove_dir_all(&repo).ok();
    fs::create_dir_all(&repo).unwrap();
    let home = std::path::PathBuf::from(std::env::var("USERPROFILE").unwrap());
    let escaped = home.join(format!("kt-sandbox-escape-{}.txt", std::process::id()));
    let state_db = board::state_db(&repo);
    // (name, command, passes). Programs by bare name: see tests/exit.rs.
    let cases = [
        ("write-checkout", "echo x> made-here.txt".to_string(), true),
        ("write-home", format!("echo x> {}", escaped.display()), false),
        ("read-board", format!("type {}", state_db.display()), false),
        ("network", "curl -sf -m 5 -o made.txt http://1.1.1.1/".to_string(), false),
    ];
    let mut config = String::new();
    for (name, run, _) in &cases {
        config.push_str(&format!("[[check]]\nname = \"{name}\"\nscope = \"repo\"\nrun = '{run}'\ninputs = [\"**\"]\n\n"));
    }
    fs::write(repo.join("kintrel.toml"), &config).unwrap();
    let root = board::init(&repo).unwrap();
    let st = State::open(&state_db).unwrap();
    let out = Ci::open(&repo).unwrap().run(&st, &repo, config_of(&st, root).unwrap(), &Snapshot { root, ..Default::default() }, None).unwrap();
    let wrong: Vec<_> = out
        .iter()
        .filter(|o| cases.iter().any(|(name, _, passes)| o.check == *name && o.ok != *passes))
        .map(|o| format!("{}: ok={} output={:?}", o.check, o.ok, o.output))
        .collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
    assert!(!escaped.exists(), "a check wrote the home directory");
    fs::remove_dir_all(board::runtime_dir(&repo).unwrap()).ok();
    fs::remove_dir_all(&repo).ok();
}
