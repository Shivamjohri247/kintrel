//! CI through a real daemon (SPEC §9.2, §10.4): gate checks block promotion, authors get
//! `ci_failed` notices fast, and a fixed head is promoted once its checks pass.

use std::time::{Duration, Instant};

use kintrel::daemon::{self, Options};
use kintrel::rpc::Client;
use kintrel_core::log::Durability;
use kintrel_core::proto::{self, FileWrite, Inbox, Notices, Post, Posted, SessionStart, SessionStarted, StatusInfo};

const CONFIG: &str = r#"
[[check]]
name = "no-fail-marker"
scope = "file"
run = "! grep -q FAIL {file}"
inputs = ["*.txt"]
gate = true

[[check]]
name = "slow"
tier = "test"
scope = "repo"
run = "sleep 1"
inputs = ["**"]
gate = true
"#;

#[tokio::test(flavor = "multi_thread")]
async fn gate_blocks_and_feedback_is_fast() {
    let repo = std::env::temp_dir().join(format!("kt-ci-daemon-{}", std::process::id()));
    std::fs::remove_dir_all(&repo).ok();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("kintrel.toml"), CONFIG).unwrap();
    std::fs::write(repo.join("a.txt"), "ok\n").unwrap();
    kintrel_core::board::init(&repo).unwrap();
    let opts = Options { repo: repo.clone(), durability: Durability::Normal, promote_every: None };
    tokio::spawn(async move { daemon::run(opts).await.unwrap() });
    let sock = daemon::socket_path(&repo).unwrap();
    while Client::connect(&sock).await.is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut c = Client::connect(&sock).await.unwrap();
    let s: SessionStarted = c.call(proto::SESSION_START, &SessionStart { agent: "a".into(), rank_hint: None, base: None }).await.unwrap();
    let post = |text: &str| Post {
        token: s.token.clone(),
        files: vec![FileWrite { path: "a.txt".into(), content: Some(text.into()), base: None }],
        summary: None,
    };

    let posted_at = Instant::now();
    let _: Posted = c.call(proto::POST, &post("FAIL here\n")).await.unwrap();
    let notice = loop {
        let n: Notices = c.call(proto::INBOX, &Inbox { token: s.token.clone() }).await.unwrap();
        if let Some(n) = n.notices.into_iter().find(|n| n.kind == "ci_failed") {
            break n;
        }
        assert!(posted_at.elapsed() < Duration::from_secs(5), "no ci_failed notice");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let signal = posted_at.elapsed();
    assert_eq!(notice.atom, "a.txt");
    assert!(notice.check.as_deref().unwrap().starts_with("no-fail-marker"), "{notice:?}");
    assert!(signal < Duration::from_secs(1), "lint signal took {signal:?} (target < 1 s)");
    eprintln!("lint signal after {signal:?}");

    // The candidate fails the gate; bisection finds the note and rejects it, and head without
    // it is promoted. The author is told, refreshes (the note is reported lost) and redoes it.
    let st: StatusInfo = c.call(proto::PROMOTE_NOW, &proto::Status {}).await.unwrap();
    let st = if st.promoted == st.head { st } else { c.call(proto::PROMOTE_NOW, &proto::Status {}).await.unwrap() };
    assert_eq!((st.promoted, st.failing.len()), (st.head, 0), "{st:?}");
    let read = |at| proto::ReadFile { token: String::new(), path: "a.txt".into(), at };
    let head: proto::FileContent = c.call(proto::READ_FILE, &read(st.head)).await.unwrap();
    assert_eq!(head.content, "ok\n", "the failing note is out of head");
    let n: Notices = c.call(proto::INBOX, &Inbox { token: s.token.clone() }).await.unwrap();
    let rejected = n.notices.iter().find(|n| n.kind == "rejected").expect("author told");
    assert_eq!(rejected.check.as_deref(), Some("no-fail-marker"));
    let r: proto::Refreshed = c.call(proto::SESSION_REFRESH, &proto::Refresh { token: s.token.clone() }).await.unwrap();
    assert_eq!(r.lost.len(), 1, "a rejected note is lost");

    let _: Posted = c.call(proto::POST, &post("fine\n")).await.unwrap();
    let st: StatusInfo = c.call(proto::PROMOTE_NOW, &proto::Status {}).await.unwrap();
    assert!(st.failing.is_empty() && st.promoted == st.head, "{st:?}");
    let head: proto::FileContent = c.call(proto::READ_FILE, &read(st.head)).await.unwrap();
    assert_eq!(head.content, "fine\n");

    // An agent rewrites kintrel.toml: dropping the gate, and adding a command. Head's config is
    // never used: nothing runs it, and promotion waits for a person to approve the file.
    let marker = std::env::temp_dir().join(format!("kt-ci-pwned-{}", std::process::id()));
    std::fs::remove_file(&marker).ok();
    let evil = format!("[[check]]\nname = \"x\"\nscope = \"repo\"\nrun = \"touch {}\"\ninputs = [\"**\"]\n", marker.display());
    let files = vec![FileWrite { path: "kintrel.toml".into(), content: Some(evil.clone()), base: None }];
    let _: Posted = c.call(proto::POST, &Post { token: s.token.clone(), files, summary: None }).await.unwrap();
    let st: StatusInfo = c.call(proto::PROMOTE_NOW, &proto::PromoteNow::default()).await.unwrap();
    assert_eq!(st.failing, [kintrel::ci_worker::UNAPPROVED_CONFIG]);
    let ran: proto::ChecksRan = c.call(proto::CHECKS_RUN, &proto::ChecksRun { token: Some(s.token.clone()), ..Default::default() }).await.unwrap();
    assert!(ran.outcomes.iter().all(|o| o.check != "x"), "checks come from the promoted config");
    tokio::time::sleep(Duration::from_millis(300)).await; // let head feedback run
    assert!(!marker.exists(), "a command from an unpromoted kintrel.toml ran");

    // Approval names the reviewed file, with a token made from the person key; a stale or wrong
    // hash, or a missing or wrong token, is refused.
    let reviewed = kintrel_core::Hash::of(evil.as_bytes());
    let key = std::fs::read_to_string(kintrel_core::board::person_key(&repo).unwrap()).unwrap();
    let token = |key: &str, h| Some(kintrel_core::board::approval(key, Some(kintrel_core::Hash::of(CONFIG.as_bytes())), h));
    let approve = |approve_config, approval| proto::PromoteNow { approve_config: Some(approve_config), approval };
    for wrong in [
        approve(kintrel_core::Hash::default(), token(&key, kintrel_core::Hash::default())),
        approve(reviewed, None),
        approve(reviewed, token("guessed", reviewed)),
    ] {
        let r: Result<StatusInfo, _> = c.call(proto::PROMOTE_NOW, &wrong).await;
        assert!(r.is_err(), "{wrong:?}");
    }
    let st: StatusInfo = c.call(proto::PROMOTE_NOW, &approve(reviewed, token(&key, reviewed))).await.unwrap();
    assert!(st.failing.is_empty() && st.promoted == st.head, "approved: {st:?}");
    std::fs::remove_file(&marker).ok();

    std::fs::remove_dir_all(kintrel_core::board::runtime_dir(&repo).unwrap()).ok();
    std::fs::remove_dir_all(&repo).ok();
}

async fn board(name: &str, config: &str, files: &[(&str, &str)]) -> (std::path::PathBuf, Client) {
    let repo = std::env::temp_dir().join(format!("kt-ci-{name}-{}", std::process::id()));
    std::fs::remove_dir_all(&repo).ok();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("kintrel.toml"), config).unwrap();
    for (p, t) in files {
        std::fs::write(repo.join(p), t).unwrap();
    }
    kintrel_core::board::init(&repo).unwrap();
    let opts = Options { repo: repo.clone(), durability: Durability::Normal, promote_every: None };
    tokio::spawn(async move { daemon::run(opts).await.unwrap() });
    let sock = daemon::socket_path(&repo).unwrap();
    while Client::connect(&sock).await.is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (repo, Client::connect(&sock).await.unwrap())
}

async fn write(c: &mut Client, agent: &str, path: &str, text: &str) -> (String, Posted) {
    let s: SessionStarted = c.call(proto::SESSION_START, &SessionStart { agent: agent.into(), rank_hint: None, base: None }).await.unwrap();
    let files = vec![FileWrite { path: path.into(), content: Some(text.into()), base: None }];
    let p: Posted = c.call(proto::POST, &Post { token: s.token.clone(), files, summary: None }).await.unwrap();
    (s.token, p)
}

#[tokio::test(flavor = "multi_thread")]
async fn bisection_rejects_only_the_culprit() {
    let config = "[[check]]\nname = \"no-fail\"\nscope = \"file\"\nrun = \"! grep -q FAIL {file}\"\ninputs = [\"*.txt\"]\ngate = true\n";
    let (repo, mut c) = board("bisect", config, &[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")]).await;
    let (_, good1) = write(&mut c, "a", "a.txt", "a2\n").await;
    let (bad_token, bad) = write(&mut c, "b", "b.txt", "FAIL\n").await;
    let (_, good2) = write(&mut c, "c", "c.txt", "c2\n").await;
    let _ = (good1, good2);
    let mut st: StatusInfo = c.call(proto::PROMOTE_NOW, &proto::Status {}).await.unwrap();
    for _ in 0..3 {
        if st.promoted == st.head {
            break;
        }
        st = c.call(proto::PROMOTE_NOW, &proto::Status {}).await.unwrap();
    }
    assert_eq!(st.promoted, st.head, "{st:?}");
    let read = |p: &str| proto::ReadFile { token: String::new(), path: p.into(), at: st.head };
    let text = |f: proto::FileContent| f.content;
    assert_eq!(text(c.call(proto::READ_FILE, &read("a.txt")).await.unwrap()), "a2\n");
    assert_eq!(text(c.call(proto::READ_FILE, &read("b.txt")).await.unwrap()), "b\n", "only the culprit is out");
    assert_eq!(text(c.call(proto::READ_FILE, &read("c.txt")).await.unwrap()), "c2\n");
    let n: Notices = c.call(proto::INBOX, &Inbox { token: bad_token }).await.unwrap();
    let r = n.notices.iter().find(|n| n.kind == "rejected").expect("culprit's author told");
    assert_eq!(r.by.as_ref().unwrap().note, bad.note_id.short());
    std::fs::remove_dir_all(kintrel_core::board::runtime_dir(&repo).unwrap()).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn deploys_and_the_backstop() {
    // Deploys are sandboxed: the log's directory must be declared writable.
    let logdir = std::env::temp_dir().join(format!("kt-ci-deploys-{}", std::process::id()));
    std::fs::remove_dir_all(&logdir).ok();
    std::fs::create_dir_all(&logdir).unwrap();
    let logdir = std::fs::canonicalize(&logdir).unwrap();
    let log = logdir.join("log");
    // `full` is not a gate check, so it never blocks promotion by itself; the backstop runs it.
    let config = format!(
        "[[check]]\nname = \"full\"\ntier = \"test\"\nscope = \"repo\"\nrun = \"! grep -q FAIL a.txt\"\ninputs = [\"*.txt\"]\n\n\
         [[deploy]]\nname = \"ship\"\nrun = \"cat a.txt >> {}\"\nwritable = [\"{}\"]\n",
        log.display(),
        logdir.display()
    );
    let (repo, mut c) = board("backstop", &config, &[("a.txt", "v0\n")]).await;
    let (token, _) = write(&mut c, "a", "a.txt", "v1\n").await;
    let st: StatusInfo = c.call(proto::PROMOTE_NOW, &proto::Status {}).await.unwrap();
    assert_eq!(st.promoted, st.head);
    let start = Instant::now();
    while std::fs::read_to_string(&log).unwrap_or_default() != "v1\n" {
        assert!(start.elapsed() < Duration::from_secs(5), "deploy did not run once on the promoted root");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // A failure only the full suite sees slips through the gate; the 20th promotion's backstop
    // run catches it and everyone is told. Until a candidate passes everything, none promotes.
    let post = |t: &str| Post { token: token.clone(), files: vec![FileWrite { path: "a.txt".into(), content: Some(t.into()), base: None }], summary: None };
    let _: Posted = c.call(proto::POST, &post("FAIL\n")).await.unwrap();
    let _: StatusInfo = c.call(proto::PROMOTE_NOW, &proto::Status {}).await.unwrap();
    for i in 2..kintrel::ci_worker::BACKSTOP_EVERY {
        let _: Posted = c.call(proto::POST, &post(&format!("FAIL {i}\n"))).await.unwrap();
        let _: StatusInfo = c.call(proto::PROMOTE_NOW, &proto::Status {}).await.unwrap();
    }
    let incident = loop {
        let n: Notices = c.call(proto::INBOX, &Inbox { token: token.clone() }).await.unwrap();
        if let Some(n) = n.notices.into_iter().find(|n| n.kind == "incident") {
            break n;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "no incident after the backstop run");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(incident.check.as_deref(), Some("full"));
    let _: Posted = c.call(proto::POST, &post("FAIL still\n")).await.unwrap();
    let st: StatusInfo = c.call(proto::PROMOTE_NOW, &proto::Status {}).await.unwrap();
    assert_ne!(st.promoted, st.head, "during an incident the full suite gates promotion");
    let _: Posted = c.call(proto::POST, &post("fixed\n")).await.unwrap();
    let st: StatusInfo = c.call(proto::PROMOTE_NOW, &proto::Status {}).await.unwrap();
    let st = if st.promoted == st.head { st } else { c.call(proto::PROMOTE_NOW, &proto::Status {}).await.unwrap() };
    assert_eq!(st.promoted, st.head, "{st:?}");
    std::fs::remove_dir_all(&logdir).ok();
    std::fs::remove_dir_all(kintrel_core::board::runtime_dir(&repo).unwrap()).ok();
    std::fs::remove_dir_all(&repo).ok();
}
