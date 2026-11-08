//! A check passes exactly when its command exits 0, on every OS, sandboxed or not. (On Windows
//! checks run in `cmd`, inside an AppContainer by default.)

use std::fs;

use kintrel_ci::{Ci, Snapshot, config_of};
use kintrel_core::board;
use kintrel_core::state::State;

#[test]
fn exit_codes_decide_pass_and_fail() {
    // (name, command, passes)
    let mut cases: Vec<(&str, &str, bool)> = if cfg!(windows) {
        vec![
            ("ok", "type marker.txt", true),
            ("exit", "exit /b 1", false),
            ("program", "findstr /C:zzz marker.txt", false),
            ("missing", "findstr /C:zzz no-such-file", false),
            // In cmd, `if c (a) & b` makes `b` part of the `if`; an `else` is the safe form.
            ("chained", "findstr /C:FAIL marker.txt & if errorlevel 1 (exit /b 0) else (exit /b 1)", false),
        ]
    } else {
        vec![("ok", "cat marker.txt", true), ("exit", "exit 1", false), ("program", "grep -q zzz marker.txt", false)]
    };
    // Known limit: an AppContainer cannot open NUL, so `>nul` fails a sandboxed check (seen on
    // Windows CI); unsandboxed it works. Pinned here so a change is noticed.
    if cfg!(windows) {
        cases.push(("nul", "echo x >nul", false));
    }
    let mut config = String::new();
    for sandbox in [true, false] {
        for (name, run, _) in &cases {
            config.push_str(&format!("[[check]]\nname = \"{name}-{sandbox}\"\nscope = \"repo\"\nrun = '{run}'\ninputs = [\"**\"]\nsandbox = {sandbox}\n\n"));
        }
    }
    let repo = std::env::temp_dir().join(format!("kt-ci-exit-{}", std::process::id()));
    fs::remove_dir_all(&repo).ok();
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("kintrel.toml"), &config).unwrap();
    fs::write(repo.join("marker.txt"), "FAIL\n").unwrap();
    let root = board::init(&repo).unwrap();
    let st = State::open(&board::state_db(&repo)).unwrap();
    let out = Ci::open(&repo).unwrap().run(&st, &repo, config_of(&st, root).unwrap(), &Snapshot { root, ..Default::default() }, None).unwrap();
    // `>nul` passes unsandboxed.
    let expect = |check: &str, passes: bool| passes || check == "nul-false";
    let wrong: Vec<_> = out
        .iter()
        .filter(|o| cases.iter().any(|(name, _, passes)| o.check.starts_with(&format!("{name}-")) && o.ok != expect(&o.check, *passes)))
        .map(|o| format!("{}: ok={} output={:?}", o.check, o.ok, o.output))
        .collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
    assert_eq!(out.len(), 2 * cases.len());
    fs::remove_dir_all(board::runtime_dir(&repo).unwrap()).ok();
    fs::remove_dir_all(&repo).ok();
}
