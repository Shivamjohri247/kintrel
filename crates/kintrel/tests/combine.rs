//! The combiner through a real daemon: posts from several sessions fold into head (SPEC §7).

use std::path::Path;
use std::time::{Duration, Instant};

use kintrel::daemon::{self, Options};
use kintrel::rpc::Client;
use kintrel_core::log::Durability;
use kintrel_core::proto::{self, FileContent, FileWrite, Post, Posted, ReadFile, SessionStart, SessionStarted, StatusInfo};

const A: &str = "def f():\n    return 1\n\n\ndef g():\n    return 2\n";

async fn session(sock: &Path, agent: &str) -> (Client, String) {
    let mut c = Client::connect(sock).await.unwrap();
    let s: SessionStarted =
        c.call(proto::SESSION_START, &SessionStart { agent: agent.into(), rank_hint: None, base: None }).await.unwrap();
    (c, s.token)
}

async fn post(c: &mut Client, token: &str, content: String) {
    let files = vec![FileWrite { path: "a.py".into(), content: Some(content), base: None }];
    let _: Posted = c.call(proto::POST, &Post { token: token.into(), files, summary: None }).await.unwrap();
}

/// Polls until every note is folded and head's `a.py` is `want` (the combiner runs
/// asynchronously; head can show `want` before a later, losing note is folded).
async fn settle(c: &mut Client, token: &str, want: &str) -> StatusInfo {
    let start = Instant::now();
    loop {
        let s: StatusInfo = c.call(proto::STATUS, &proto::Status {}).await.unwrap();
        let read = ReadFile { token: token.into(), path: "a.py".into(), at: s.head };
        let head: FileContent = c.call(proto::READ_FILE, &read).await.unwrap();
        if head.content == want && s.folded == s.notes {
            return s;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "head never became:\n{want}\nlast:\n{}", head.content);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_fold_into_head() {
    let repo = std::env::temp_dir().join(format!("kt-combine-{}", std::process::id()));
    std::fs::remove_dir_all(&repo).ok();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("a.py"), A).unwrap();
    kintrel_core::board::init(&repo).unwrap();
    // Promotion only on request, so rank (not timing) decides between close-together notes.
    let opts = Options { repo: repo.clone(), durability: Durability::Normal, promote_every: None };
    tokio::spawn(async move { daemon::run(opts).await.unwrap() });
    let sock = daemon::socket_path(&repo).unwrap();
    while Client::connect(&sock).await.is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Disjoint edits from two sessions both land.
    let (mut a, ta) = session(&sock, "a").await;
    let (mut b, tb) = session(&sock, "b").await;
    post(&mut b, &tb, A.replace("return 2", "return 20")).await;
    post(&mut a, &ta, A.replace("return 1", "return 10")).await;
    let want = A.replace("return 1", "return 10").replace("return 2", "return 20");
    assert_eq!(settle(&mut a, &ta, &want).await.conflicts, 0);

    // Two new sessions (based on that head) edit the same line: the lower rank wins.
    let (mut c, tc) = session(&sock, "c").await;
    let (mut d, td) = session(&sock, "d").await;
    post(&mut d, &td, want.replace("return 10", "return 12")).await;
    post(&mut c, &tc, want.replace("return 10", "return 11")).await;
    let s = settle(&mut c, &tc, &want.replace("return 10", "return 11")).await;
    assert_eq!(s.conflicts, 1);

    // Causality beats rank: a (rank 1) refreshes onto b's edit and builds on it; b (rank 2)
    // then edits the same line from its stale view. a's note must fold after b's first note
    // and win; b's stale note is the one that conflicts.
    let now = want.replace("return 10", "return 11");
    let _: proto::Refreshed = b.call(proto::SESSION_REFRESH, &proto::Refresh { token: tb.clone() }).await.unwrap();
    post(&mut b, &tb, now.replace("return 20", "return 30")).await;
    let after_b = now.replace("return 20", "return 30");
    settle(&mut a, &ta, &after_b).await;
    let r: proto::Refreshed = a.call(proto::SESSION_REFRESH, &proto::Refresh { token: ta.clone() }).await.unwrap();
    assert!(r.lost.is_empty());
    post(&mut a, &ta, after_b.replace("return 30", "return 31")).await;
    post(&mut b, &tb, after_b.replace("return 30", "return 32")).await;
    let s = settle(&mut a, &ta, &after_b.replace("return 30", "return 31")).await;
    assert_eq!(s.conflicts, 2, "c's earlier loss plus b's stale edit");
    let s: StatusInfo = a.call(proto::PROMOTE_NOW, &proto::Status {}).await.unwrap();
    assert_eq!(s.promoted, s.head, "everything parses, so head is promoted");

    // A file that does not parse blocks promotion until a later note fixes it.
    let good = after_b.replace("return 30", "return 31");
    let _: proto::Refreshed = a.call(proto::SESSION_REFRESH, &proto::Refresh { token: ta.clone() }).await.unwrap();
    let broken = good.replace("def g():", "def g(:");
    post(&mut a, &ta, broken.clone()).await;
    settle(&mut a, &ta, &broken).await;
    let s: StatusInfo = a.call(proto::PROMOTE_NOW, &proto::Status {}).await.unwrap();
    assert_eq!(s.blocked, ["a.py"]);
    assert_ne!(s.promoted, s.head);
    let fixed = good.replace("return 31", "return 41");
    post(&mut a, &ta, fixed.clone()).await;
    settle(&mut a, &ta, &fixed).await;
    let s: StatusInfo = a.call(proto::PROMOTE_NOW, &proto::Status {}).await.unwrap();
    assert!(s.blocked.is_empty());
    assert_eq!(s.promoted, s.head);
    std::fs::remove_dir_all(kintrel_core::board::runtime_dir(&repo).unwrap()).ok();
    std::fs::remove_dir_all(&repo).ok();
}
