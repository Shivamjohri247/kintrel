//! Tier 3 through a real daemon (SPEC §7.3), with a stand-in for the model: a new conflict gets a
//! proposal, posted as a Resolution note that lands in head and resolves the conflict.
#![cfg(unix)]

use std::time::{Duration, Instant};

use kintrel::daemon::{self, Options};
use kintrel::rpc::Client;
use kintrel_core::log::Durability;
use kintrel_core::proto::{self, FileContent, FileWrite, Inbox, Notices, Post, Posted, ReadFile, SessionStart, SessionStarted, StatusInfo};

const A: &str = "def fee():\n    return 1\n";

async fn session(sock: &std::path::Path, agent: &str) -> (Client, String) {
    let mut c = Client::connect(sock).await.unwrap();
    let s: SessionStarted = c.call(proto::SESSION_START, &SessionStart { agent: agent.into(), rank_hint: None, base: None }).await.unwrap();
    (c, s.token)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conflict_gets_a_proposal_that_resolves_it() {
    let repo = std::env::temp_dir().join(format!("kt-tier3-{}", std::process::id()));
    std::fs::remove_dir_all(&repo).ok();
    std::fs::create_dir_all(&repo).unwrap();
    // The stand-in model ignores the prompt and merges both edits into `return 23`.
    let model = repo.with_extension("model.sh");
    std::fs::write(&model, "cat > /dev/null\nprintf '<<<KINTREL-MERGED\\ndef fee():\\n    return 23\\nKINTREL-MERGED>>>\\n'\n").unwrap();
    std::fs::write(
        repo.join("kintrel.toml"),
        format!("[merge]\ntier3 = true\ntier3_command = [\"sh\", \"{}\"]\n", model.display()),
    )
    .unwrap();
    std::fs::write(repo.join("a.py"), A).unwrap();
    kintrel_core::board::init(&repo).unwrap();
    let opts = Options { repo: repo.clone(), durability: Durability::Normal, promote_every: None };
    tokio::spawn(async move { daemon::run(opts).await.unwrap() });
    let sock = daemon::socket_path(&repo).unwrap();
    while Client::connect(&sock).await.is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut c = Client::connect(&sock).await.unwrap();
    let (mut d, td) = session(&sock, "d").await;
    let (mut e, te) = session(&sock, "e").await;
    let post = |token: &str, text: &str| Post {
        token: token.into(),
        files: vec![FileWrite { path: "a.py".into(), content: Some(text.into()), base: None, binary: None }],
        summary: None,
    };
    let _: Posted = e.call(proto::POST, &post(&te, &A.replace("return 1", "return 3"))).await.unwrap();
    let _: Posted = d.call(proto::POST, &post(&td, &A.replace("return 1", "return 2"))).await.unwrap();

    let began = Instant::now();
    loop {
        let s: StatusInfo = c.call(proto::STATUS, &proto::Status {}).await.unwrap();
        let head: FileContent =
            c.call(proto::READ_FILE, &ReadFile { token: String::new(), path: "a.py".into(), at: s.head }).await.unwrap();
        if head.content.contains("return 23") && s.conflicts == 0 && s.folded == s.notes {
            break;
        }
        assert!(began.elapsed() < Duration::from_secs(10), "no proposal landed: {head:?} {s:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let n: Notices = e.call(proto::INBOX, &Inbox { token: te.clone() }).await.unwrap();
    assert!(n.notices.iter().any(|n| n.kind == "merge_proposed"), "{:?}", n.notices);
    std::fs::remove_file(&model).ok();
    std::fs::remove_dir_all(kintrel_core::board::runtime_dir(&repo).unwrap()).ok();
    std::fs::remove_dir_all(&repo).ok();
}
