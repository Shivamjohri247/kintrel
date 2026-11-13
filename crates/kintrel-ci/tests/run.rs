//! Checks run in a fresh checkout, are cached by content, and see only allowlisted env.
//! The checks here are `sh` commands, so these run on Unix; Windows needs cmd equivalents.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;

use kintrel_ci::{Ci, Snapshot, Tier, config_of};
use kintrel_core::board;
use kintrel_core::state::State;
use kintrel_core::tree::Kind;

const CONFIG: &str = r#"
[[check]]
name = "nonempty"
scope = "file"
run = "test -s {file}"
inputs = ["*.txt"]
gate = true

[[check]]
name = "env"
tier = "test"
scope = "repo"
run = "env"
inputs = ["**"]
"#;

#[test]
fn cached_by_content_with_a_clean_env() {
    let repo = std::env::temp_dir().join(format!("kt-ci-{}", std::process::id()));
    fs::remove_dir_all(&repo).ok();
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("kintrel.toml"), CONFIG).unwrap();
    fs::write(repo.join("a.txt"), "x").unwrap();
    fs::write(repo.join("b.txt"), "").unwrap();
    let root = board::init(&repo).unwrap();
    let st = State::open(&board::state_db(&repo)).unwrap();
    let ci = Ci::open(&repo).unwrap();
    let snap = Snapshot { root, ..Default::default() };
    let view = |o: &[kintrel_ci::Outcome]| -> Vec<(String, String, bool, bool)> {
        let mut v: Vec<_> = o.iter().map(|o| (o.check.clone(), o.target.clone(), o.ok, o.cached)).collect();
        v.sort();
        v
    };

    let first = ci.run(&st, &repo, config_of(&st, root).unwrap(), &snap, None).unwrap();
    assert_eq!(view(&first), [
        ("env".into(), "".into(), true, false),
        ("nonempty".into(), "a.txt".into(), true, false),
        ("nonempty".into(), "b.txt".into(), false, false),
    ]);
    let env = &first.iter().find(|o| o.check == "env").unwrap().output;
    assert!(env.contains("PATH="), "{env}");
    assert!(!env.contains("CARGO"), "undeclared variables must not reach checks:\n{env}");

    let again = ci.run(&st, &repo, config_of(&st, root).unwrap(), &snap, None).unwrap();
    assert!(again.iter().all(|o| o.cached), "an unchanged snapshot is fully cached");

    // A session's own write to b.txt: only what depends on b.txt reruns.
    let fixed = st.put_blob(b"y").unwrap();
    let snap2 = Snapshot { root, overlay: BTreeMap::from([("b.txt".to_owned(), Some((Kind::File, fixed)))]), ..Default::default() };
    assert_eq!(view(&ci.run(&st, &repo, config_of(&st, root).unwrap(), &snap2, None).unwrap()), [
        ("env".into(), "".into(), true, false),
        ("nonempty".into(), "a.txt".into(), true, true),
        ("nonempty".into(), "b.txt".into(), true, false),
    ]);
    assert_eq!(ci.run(&st, &repo, config_of(&st, root).unwrap(), &snap2, Some(Tier::Lint)).unwrap().len(), 2, "tier filter");

    fs::remove_dir_all(board::runtime_dir(&repo).unwrap()).ok();
    fs::remove_dir_all(&repo).ok();
}

#[test]
fn timeouts_are_reported_and_not_cached() {
    let repo = std::env::temp_dir().join(format!("kt-ci-timeout-{}", std::process::id()));
    fs::remove_dir_all(&repo).ok();
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("kintrel.toml"), "[[check]]\nname = \"slow\"\nscope = \"repo\"\nrun = \"sleep 5\"\ninputs = [\"**\"]\ntimeout_s = 1\n").unwrap();
    let root = board::init(&repo).unwrap();
    let st = State::open(&board::state_db(&repo)).unwrap();
    let ci = Ci::open(&repo).unwrap();
    let snap = Snapshot { root, ..Default::default() };
    let o = ci.run(&st, &repo, config_of(&st, root).unwrap(), &snap, None).unwrap();
    assert!(o[0].timed_out && !o[0].ok && o[0].ms < 3000, "{o:?}");
    assert!(!ci.run(&st, &repo, config_of(&st, root).unwrap(), &snap, None).unwrap()[0].cached, "a timeout may be flaky: never cached");
    fs::remove_dir_all(board::runtime_dir(&repo).unwrap()).ok();
    fs::remove_dir_all(&repo).ok();
}

#[test]
fn config_errors_are_explicit() {
    use kintrel_ci::Config;
    let bad = |t: &str| Config::parse(t).unwrap_err().to_string();
    assert!(bad("[[check]]\nname = \"a\"\nscope = \"repo\"\nrun = \"x {file}\"\ninputs = [\"*\"]\n").contains("{file}"));
    assert!(bad("[[check]]\nname = \"a\"\nscope = \"repo\"\nrun = \"x {package}\"\ninputs = [\"*\"]\n").contains("package"));
    assert!(bad("[[check]]\nname = \"a\"\nscope = \"repo\"\nrun = \"x\"\ninputs = [\"*\"]\n[[check]]\nname = \"a\"\nscope = \"repo\"\nrun = \"y\"\ninputs = [\"*\"]\n").contains("unique"));
    assert!(bad("[[check]]\nname = \"a\"\nscope = \"repo\"\nrun = \"x\"\ninputs = [\"*\"]\ntypo = 1\n").contains("typo"));
}

#[test]
fn early_cutoff_between_packages() {
    let repo = std::env::temp_dir().join(format!("kt-ci-pkg-{}", std::process::id()));
    fs::remove_dir_all(&repo).ok();
    for d in ["core", "app"] {
        fs::create_dir_all(repo.join(d)).unwrap();
        fs::write(repo.join(d).join("pyproject.toml"), format!("[project]\nname = \"{d}\"\n")).unwrap();
    }
    fs::write(repo.join("core/lib.py"), "def f(x):\n    return x\n").unwrap();
    fs::write(repo.join("app/main.py"), "import core\n\n\ndef run():\n    return core.f(1)\n").unwrap();
    fs::write(repo.join("deps.lock"), "v1\n").unwrap();
    fs::write(
        repo.join("kintrel.toml"),
        "[[check]]\nname = \"types\"\ntier = \"typecheck\"\nscope = \"package\"\nrun = \"test -d {package_dir} && echo {package}\"\ninputs = [\"**/*.py\", \"*.lock\"]\n\n\
         [[check]]\nname = \"tests\"\ntier = \"test\"\nscope = \"package\"\nrun = \"true\"\ninputs = [\"**/*.py\", \"*.lock\"]\n",
    )
    .unwrap();
    let root = board::init(&repo).unwrap();
    let st = State::open(&board::state_db(&repo)).unwrap();
    let ci = Ci::open(&repo).unwrap();
    // (check, package dir) pairs that actually ran (were not cached).
    let ran = |overlay: &[(&str, &str)]| -> Vec<(String, String)> {
        let overlay = overlay.iter().map(|(p, t)| (p.to_string(), Some((Kind::File, st.put_blob(t.as_bytes()).unwrap())))).collect();
        let out = ci.run(&st, &repo, config_of(&st, root).unwrap(), &Snapshot { root, overlay, ..Default::default() }, None).unwrap();
        assert!(out.iter().all(|o| o.ok), "{out:?}");
        let mut v: Vec<_> = out.iter().filter(|o| !o.cached).map(|o| (o.check.clone(), o.target.clone())).collect();
        v.sort();
        v
    };
    let pair = |c: &str, p: &str| (c.to_owned(), p.to_owned());
    assert_eq!(ran(&[]).len(), 4);
    assert_eq!(
        ran(&[("core/lib.py", "def f(x):\n    return x + 1\n")]),
        [pair("tests", "app"), pair("tests", "core"), pair("types", "core")],
        "a body edit in core: app's type check is cut off, app's tests are not"
    );
    assert_eq!(
        ran(&[("core/lib.py", "def f(x, y=0):\n    return x\n")]),
        [pair("tests", "app"), pair("tests", "core"), pair("types", "app"), pair("types", "core")],
        "a signature edit reruns the dependent's type check"
    );
    assert_eq!(ran(&[("deps.lock", "v2\n")]).len(), 4, "a root lockfile no package owns concerns every package");
    fs::remove_dir_all(board::runtime_dir(&repo).unwrap()).ok();
    fs::remove_dir_all(&repo).ok();
}

/// The real sandbox, macOS (sandbox-exec) or Linux (bubblewrap, which must be installed).
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn sandbox_confines_checks() {
    let repo = std::env::temp_dir().join(format!("kt-ci-sandbox-{}", std::process::id()));
    fs::remove_dir_all(&repo).ok();
    fs::create_dir_all(&repo).unwrap();
    let home = std::path::PathBuf::from(std::env::var("HOME").unwrap());
    let escaped = home.join(format!("kt-sandbox-escape-{}", std::process::id()));
    let allowed = home.join(format!("kt-sandbox-optout-{}", std::process::id()));
    // A socket like the daemon's, outside the checkout (short path: macOS caps them at 104 bytes).
    let sock_path = format!("/tmp/kt-sb-{}.sock", std::process::id());
    std::fs::remove_file(&sock_path).ok();
    let _listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
    let config = format!(
        r#"
[[check]]
name = "write-checkout"
scope = "repo"
run = "touch made-here && test -f made-here"
inputs = ["**"]

[[check]]
name = "write-home"
scope = "repo"
run = "touch {escaped}"
inputs = ["**"]

[[check]]
name = "read-secrets"
scope = "repo"
run = "ls ~/.ssh"
inputs = ["**"]

[[check]]
name = "read-board"
scope = "repo"
run = "test -e {board}/state.db"
inputs = ["**"]

[[check]]
name = "loopback"
scope = "repo"
# Retries while the server starts: a fixed wait flakes on a slow runner.
run = "python3 -m http.server 8799 --bind 127.0.0.1 >/dev/null 2>&1 & s=$!; r=1; for i in $(seq 100); do curl -sf -o /dev/null http://127.0.0.1:8799/ && {{ r=0; break; }}; sleep 0.1; done; kill $s; exit $r"
inputs = ["**"]

[[check]]
name = "external"
scope = "repo"
run = "curl -sf -m 3 -o /dev/null http://1.1.1.1/"
inputs = ["**"]

[[check]]
name = "outside-socket"
scope = "repo"
run = '''python3 -c 'import socket; s = socket.socket(socket.AF_UNIX); s.connect("{sock}")' '''
inputs = ["**"]
network = true

[[check]]
name = "own-socket"
scope = "repo"
run = '''python3 -c 'import os, socket; p = os.environ["TMPDIR"] + "/s"; l = socket.socket(socket.AF_UNIX); l.bind(p); l.listen(); c = socket.socket(socket.AF_UNIX); c.connect(p)' '''
inputs = ["**"]

[[check]]
name = "opted-out"
scope = "repo"
run = "touch {allowed}"
inputs = ["**"]
sandbox = false

[[check]]
name = "bad-writable"
scope = "repo"
run = "true"
inputs = ["**"]
writable = ["$NOT_LISTED/x"]
"#,
        escaped = escaped.display(),
        allowed = allowed.display(),
        sock = sock_path,
        board = board::data_dir(&repo).display()
    );
    fs::write(repo.join("kintrel.toml"), &config).unwrap();
    let root = board::init(&repo).unwrap();
    let st = State::open(&board::state_db(&repo)).unwrap();
    let ci = Ci::open(&repo).unwrap();
    let out = ci.run(&st, &repo, config_of(&st, root).unwrap(), &Snapshot { root, ..Default::default() }, None).unwrap();
    let ok = |name: &str| out.iter().find(|o| o.check == name).unwrap_or_else(|| panic!("{name} ran")).ok;
    assert!(ok("write-checkout"), "{out:?}");
    assert!(!ok("write-home") && !escaped.exists(), "writes outside the checkout are denied");
    assert!(!home.join(".ssh").is_dir() || !ok("read-secrets"), "~/.ssh is hidden");
    assert!(!ok("read-board"), "the board's .kintrel (the person key) is hidden");
    assert!(ok("loopback"), "loopback stays open for test servers");
    assert!(!ok("external"), "no network beyond loopback");
    assert!(ok("opted-out") && allowed.exists(), "sandbox = false runs unconfined");
    assert!(!ok("outside-socket"), "no outside Unix socket, even with network on (the daemon's is one)");
    assert!(ok("own-socket"), "sockets in the check's own TMPDIR work");
    std::fs::remove_file(&sock_path).ok();
    let bad = out.iter().find(|o| o.check == "bad-writable").unwrap();
    assert!(!bad.ok && bad.infra_error && bad.output.contains("NOT_LISTED"), "{bad:?}");
    fs::remove_file(&allowed).ok();
    fs::remove_dir_all(board::runtime_dir(&repo).unwrap()).ok();
    fs::remove_dir_all(&repo).ok();
}

/// An agent works in its view but cannot read a person key, write the board, write elsewhere
/// (where it could plant something the person runs later), or reach Unix sockets other than
/// the daemon's (a tmux server would run its commands outside the sandbox).
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn agents_cannot_act_as_the_person() {
    // Under home, not temp: the temp directories are writable to agents.
    let root = std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(format!("kt-agent-{}", std::process::id()));
    let (repo, view, keys, run_dir) = (root.join("repo"), root.join("view"), root.join("keys"), root.join("run"));
    let board = repo.join(".kintrel");
    for d in [&board, &view, &keys, &run_dir] {
        fs::create_dir_all(d).unwrap();
    }
    let key = keys.join("board1");
    fs::write(&key, "the-person-key").unwrap();
    // Sockets: the daemon's (outside /tmp, as the real one is), and another in /tmp, where tmux
    // keeps its server's (refused on macOS; on Linux /tmp is private).
    let (daemon_sock, other_sock) = (format!("{}/d.sock", run_dir.display()), format!("/tmp/kt-ao-{}.sock", std::process::id()));
    let _d = std::os::unix::net::UnixListener::bind(&daemon_sock).unwrap();
    let _o = std::os::unix::net::UnixListener::bind(&other_sock).unwrap();
    let policy = kintrel_ci::sandbox::AgentPolicy {
        view: &view,
        ours: &[],
        extra: &[],
        board: &board,
        keys: &keys,
        socket: std::path::Path::new(&daemon_sock),
    };
    let run = |script: String| {
        let argv = ["/bin/sh", "-c", &script].map(std::ffi::OsString::from);
        let out = kintrel_ci::sandbox::agent(&argv, &policy).unwrap().output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let p = |p: &std::path::Path| p.display().to_string();
    let out = run(format!(
        "touch {v}/ok; cat {k}; ln {k} {v}/link; cat {v}/link; ls {keys}; touch {b}/x {r}/x {root}/x",
        v = p(&view),
        k = p(&key),
        keys = p(&keys),
        b = p(&board),
        r = p(&repo),
        root = p(&root)
    ));
    assert!(view.join("ok").exists(), "the agent writes its view");
    assert!(!out.contains("the-person-key") && !out.contains("board1"), "a person key leaked: {out}");
    assert!(!board.join("x").exists() && !repo.join("x").exists() && !root.join("x").exists(), "wrote outside its view");
    let connect = |sock: &str| run(format!("python3 -I -c 'import socket; socket.socket(socket.AF_UNIX).connect(\"{sock}\"); print(\"connected\")'"));
    assert!(connect(&daemon_sock).contains("connected"), "the daemon's socket is reachable");
    assert!(!connect(&other_sock).contains("connected"), "another socket is not");
    // Nor through a link in the view (Seatbelt resolves symlinks and refuses the hard link;
    // on Linux the outside socket is not there at all).
    let linked = format!("{}/l.sock", view.display());
    run(format!("ln -s {other_sock} {linked}"));
    assert!(!connect(&linked).contains("connected"), "through a symlink");
    fs::remove_file(&linked).ok();
    run(format!("ln {other_sock} {linked}"));
    assert!(!connect(&linked).contains("connected"), "through a hard link");
    // No keystrokes pushed into the person's terminal (a pseudo-terminal here, through `script`).
    if cfg!(target_os = "macos") {
        let sti = "import fcntl, termios\ntry:\n    fcntl.ioctl(0, termios.TIOCSTI, b'x'); print('sti-allowed')\nexcept OSError: print('sti-refused')";
        let out = run(format!("script -q /dev/null python3 -I -c \"{sti}\" </dev/null"));
        assert!(out.contains("sti-refused"), "{out}");
    }
    fs::remove_file(&other_sock).ok();
    fs::remove_dir_all(&root).ok();
}
