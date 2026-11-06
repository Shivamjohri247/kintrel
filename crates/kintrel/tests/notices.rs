//! Notices through a real daemon (SPEC §8): readers hear about body and signature changes,
//! code that calls a changed signature hears about it, and a losing note's author hears why.

use std::path::Path;
use std::time::{Duration, Instant};

use kintrel::daemon::{self, Options};
use kintrel::rpc::Client;
use kintrel_core::log::Durability;
use kintrel_core::proto::{self, FileWrite, Inbox, Notice, Notices, Post, Posted, ReadFile, SessionStart, SessionStarted, StatusInfo};

const A: &str = "def price(x):\n    return x\n\n\ndef fee():\n    return 1\n";

async fn session(sock: &Path, agent: &str) -> (Client, String) {
    let mut c = Client::connect(sock).await.unwrap();
    let s: SessionStarted =
        c.call(proto::SESSION_START, &SessionStart { agent: agent.into(), rank_hint: None, base: None }).await.unwrap();
    (c, s.token)
}

async fn post(c: &mut Client, token: &str, path: &str, content: &str) {
    let files = vec![FileWrite { path: path.into(), content: Some(content.into()), base: None }];
    let _: Posted = c.call(proto::POST, &Post { token: token.into(), files, summary: None }).await.unwrap();
}

/// Waits until every note is folded and the router has had a moment, then takes the inbox.
async fn inbox(c: &mut Client, token: &str) -> Vec<Notice> {
    let start = Instant::now();
    loop {
        let s: StatusInfo = c.call(proto::STATUS, &proto::Status {}).await.unwrap();
        if s.folded == s.notes {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "combiner never caught up");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let n: Notices = c.call(proto::INBOX, &Inbox { token: token.into() }).await.unwrap();
    n.notices
}

fn kinds(n: &[Notice]) -> Vec<(String, String)> {
    n.iter().map(|n| (n.kind.clone(), n.atom.clone())).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn notices_reach_readers_dependents_and_losers() {
    let repo = std::env::temp_dir().join(format!("kt-notices-{}", std::process::id()));
    std::fs::remove_dir_all(&repo).ok();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("a.py"), A).unwrap();
    kintrel_core::board::init(&repo).unwrap();
    let opts = Options { repo: repo.clone(), durability: Durability::Normal, promote_every: None };
    tokio::spawn(async move { daemon::run(opts).await.unwrap() });
    let sock = daemon::socket_path(&repo).unwrap();
    while Client::connect(&sock).await.is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // a reads a.py; c writes code that calls price(). b changes fee's body.
    let (mut a, ta) = session(&sock, "a").await;
    let (mut b, tb) = session(&sock, "b").await;
    let (mut c, tc) = session(&sock, "c").await;
    let _: proto::FileContent =
        a.call(proto::READ_FILE, &ReadFile { token: ta.clone(), path: "a.py".into(), at: None }).await.unwrap();
    post(&mut c, &tc, "shop.py", "def checkout():\n    return price(3)\n").await;
    post(&mut b, &tb, "a.py", &A.replace("return 1", "return 2")).await;
    let got = inbox(&mut a, &ta).await;
    assert_eq!(kinds(&got), [("body_changed".into(), "a.py::def fee".into())]);
    assert_eq!(got[0].by.as_ref().unwrap().agent, "b");
    assert_eq!((got[0].before.as_deref(), got[0].after.as_deref()), (None, None), "no signatures for a body change");
    assert!(inbox(&mut c, &tc).await.is_empty(), "c never touched a.py and fee's signature did not change");
    assert!(inbox(&mut b, &tb).await.is_empty(), "authors are not told about their own notes");

    // b changes price's signature: a (reader) and c (caller) both hear about it.
    let v2 = A.replace("return 1", "return 2");
    post(&mut b, &tb, "a.py", &v2.replace("def price(x):", "def price(x, currency):")).await;
    let got = inbox(&mut a, &ta).await;
    assert_eq!(kinds(&got), [("interface_changed".into(), "a.py::def price".into())]);
    assert_eq!(got[0].before.as_deref(), Some("def price(x):"));
    assert_eq!(got[0].after.as_deref(), Some("def price(x, currency):"));
    let got = inbox(&mut c, &tc).await;
    assert_eq!(kinds(&got), [("interface_changed".into(), "a.py::def price".into())]);
    assert_eq!(got[0].you_affected, ["shop.py::def checkout"]);

    // Two sessions on the same head edit the same line: the higher rank loses and is told why.
    let (mut d, td) = session(&sock, "d").await;
    let (mut e, te) = session(&sock, "e").await;
    let v3 = v2.replace("def price(x):", "def price(x, currency):");
    post(&mut e, &te, "a.py", &v3.replace("return 2", "return 30")).await;
    post(&mut d, &td, "a.py", &v3.replace("return 2", "return 20")).await;
    let got = inbox(&mut e, &te).await;
    let conflict = got.iter().find(|n| n.kind == "conflict").expect("e lost");
    assert_eq!(conflict.atom, "a.py::def fee");
    assert_eq!(conflict.kept.as_deref(), Some("def fee():\n    return 20"));
    assert_eq!(conflict.theirs.as_deref(), Some("def fee():\n    return 30"));
    assert_eq!(conflict.base.as_deref(), Some("def fee():\n    return 2"));

    // e refreshes and redoes its change on top of head: the conflict is resolved.
    let open = |s: StatusInfo| s.conflicts;
    let before: StatusInfo = e.call(proto::STATUS, &proto::Status {}).await.unwrap();
    assert!(open(before) >= 1);
    let _: proto::Refreshed = e.call(proto::SESSION_REFRESH, &proto::Refresh { token: te.clone() }).await.unwrap();
    post(&mut e, &te, "a.py", &v3.replace("return 2", "return 30")).await;
    inbox(&mut e, &te).await;
    let after: StatusInfo = e.call(proto::STATUS, &proto::Status {}).await.unwrap();
    assert_eq!(after.conflicts, 0, "e's redo resolved its conflict");

    // g's view predates d's deletion of price(). g never read a.py through the API (a Claude
    // Code agent reads files directly), so no pass event reached it; its first write to a.py
    // catches it up.
    let (mut g, tg) = session(&sock, "g").await;
    let v4 = v3.replace("return 2", "return 20");
    let no_price = v4.replace("def price(x, currency):\n    return x\n\n\n", "");
    post(&mut d, &td, "a.py", &no_price).await;
    inbox(&mut d, &td).await;
    post(&mut g, &tg, "a.py", &v4.replace("return 20", "return 21")).await;
    let n: Notices = g.call(proto::INBOX, &Inbox { token: tg.clone() }).await.unwrap();
    assert_eq!(kinds(&n.notices), [("deleted".into(), "a.py::def price".into())]);

    // k's view predates d changing fee's signature. k never touches a.py: it writes a new file
    // that calls fee(). It is told at once (B4: the usual way agents depend on each other).
    let (mut k, tk) = session(&sock, "k").await;
    post(&mut d, &td, "a.py", &no_price.replace("def fee():", "def fee(rate):")).await;
    inbox(&mut d, &td).await;
    post(&mut k, &tk, "bill.py", "def bill():\n    return fee()\n").await;
    let n: Notices = k.call(proto::INBOX, &Inbox { token: tk.clone() }).await.unwrap();
    assert_eq!(kinds(&n.notices), [("interface_changed".into(), "a.py::def fee".into())]);
    assert_eq!(n.notices[0].you_affected, ["bill.py::def bill"]);
    assert_eq!(n.notices[0].after.as_deref(), Some("def fee(rate):"));

    // Same for a body-only change: m writes code using fee() after its body changed.
    let (mut m, tm) = session(&sock, "m").await;
    let fee2 = no_price.replace("def fee():", "def fee(rate):");
    post(&mut d, &td, "a.py", &fee2.replace("return 20", "return 21")).await;
    inbox(&mut d, &td).await;
    post(&mut m, &tm, "tax.py", "def tax():\n    return fee(2)\n").await;
    let n: Notices = m.call(proto::INBOX, &Inbox { token: tm.clone() }).await.unwrap();
    assert_eq!(kinds(&n.notices), [("body_changed".into(), "a.py::def fee".into())]);
    assert_eq!(n.notices[0].you_affected, ["tax.py::def tax"]);

    std::fs::remove_dir_all(kintrel_core::board::runtime_dir(&repo).unwrap()).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn claims_over_rpc() {
    let repo = std::env::temp_dir().join(format!("kt-claims-{}", std::process::id()));
    std::fs::remove_dir_all(&repo).ok();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("a.py"), A).unwrap();
    kintrel_core::board::init(&repo).unwrap();
    let opts = Options { repo: repo.clone(), durability: Durability::Normal, promote_every: None };
    tokio::spawn(async move { daemon::run(opts).await.unwrap() });
    let sock = daemon::socket_path(&repo).unwrap();
    while Client::connect(&sock).await.is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (mut a, ta) = session(&sock, "a").await;
    let (mut b, tb) = session(&sock, "b").await;
    let claim = |token: &str, targets: &[&str]| proto::Announce {
        token: token.into(),
        summary: "fee rework".into(),
        targets: targets.iter().map(|t| t.to_string()).collect(),
        lease_ms: None,
    };
    let r: proto::Announced = a.call(proto::CLAIM, &claim(&ta, &["a.py::def fee"])).await.unwrap();
    assert!(r.overlapping.is_empty());
    let bad: Result<proto::Announced, _> = a.call(proto::CLAIM, &claim(&ta, &["../x.py::def f"])).await;
    assert!(bad.is_err(), "unsafe paths are rejected");

    post(&mut b, &tb, "a.py", &A.replace("return 1", "return 2")).await;
    let got = inbox(&mut b, &tb).await;
    let n = got.iter().find(|n| n.kind == "claim_overlap").expect("b is told about a's claim");
    assert_eq!((n.atom.as_str(), n.summary.as_deref()), ("a.py::def fee", Some("fee rework")));
    assert!(inbox(&mut a, &ta).await.iter().any(|n| n.kind == "claim_overlap"));

    let _: proto::Status = a.call(proto::RELEASE, &proto::Release { token: ta.clone(), id: r.id }).await.unwrap();
    let _: proto::Status = a.call(proto::SESSION_END, &proto::SessionRef { token: ta.clone() }).await.unwrap();
    let gone: Result<Notices, _> = a.call(proto::INBOX, &Inbox { token: ta.clone() }).await;
    assert!(gone.is_err(), "an ended session's token is invalid");

    std::fs::remove_dir_all(kintrel_core::board::runtime_dir(&repo).unwrap()).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn revert_undoes_only_that_note() {
    let repo = std::env::temp_dir().join(format!("kt-revert-{}", std::process::id()));
    std::fs::remove_dir_all(&repo).ok();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("a.py"), A).unwrap();
    kintrel_core::board::init(&repo).unwrap();
    let opts = Options { repo: repo.clone(), durability: Durability::Normal, promote_every: None };
    tokio::spawn(async move { daemon::run(opts).await.unwrap() });
    let sock = daemon::socket_path(&repo).unwrap();
    while Client::connect(&sock).await.is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (mut a, ta) = session(&sock, "a").await;
    let files = |c: &str| vec![FileWrite { path: "a.py".into(), content: Some(c.into()), base: None }];
    let fee = A.replace("return 1", "return 2");
    let n1: Posted = a.call(proto::POST, &Post { token: ta.clone(), files: files(&fee), summary: None }).await.unwrap();
    let both = fee.replace("return x", "return x * 2");
    let _: Posted = a.call(proto::POST, &Post { token: ta.clone(), files: files(&both), summary: None }).await.unwrap();

    let revert = |note: String| proto::Revert { token: ta.clone(), note };
    let r: Posted = a.call(proto::REVERT, &revert(n1.note_id.short())).await.unwrap();
    assert_eq!(r.atoms, ["a.py::def fee"]);
    let read = ReadFile { token: ta.clone(), path: "a.py".into(), at: None };
    let now: proto::FileContent = a.call(proto::READ_FILE, &read).await.unwrap();
    assert_eq!(now.content, A.replace("return x", "return x * 2"), "fee is back, price keeps its later change");

    let again: Result<Posted, _> = a.call(proto::REVERT, &revert(n1.note_id.to_string())).await;
    assert!(again.is_err(), "nothing left to revert");

    // fee changes again; undoing the revert would clobber that, so it is refused.
    let _: Posted = a.call(proto::POST, &Post { token: ta.clone(), files: files(&now.content.replace("return 1", "return 9")), summary: None }).await.unwrap();
    let e = a.call::<_, Posted>(proto::REVERT, &revert(r.note_id.short())).await.unwrap_err();
    assert!(e.to_string().contains("a.py::def fee"), "{e}");

    std::fs::remove_dir_all(kintrel_core::board::runtime_dir(&repo).unwrap()).ok();
    std::fs::remove_dir_all(&repo).ok();
}
