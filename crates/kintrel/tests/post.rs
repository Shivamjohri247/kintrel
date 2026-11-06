//! End-to-end post path through a real daemon socket (SPEC §6, §4.5, §8.2).

use std::path::PathBuf;
use std::time::Duration;

use kintrel::daemon::{self, Options};
use kintrel::rpc::{CallError, Client};
use kintrel_core::Hash;
use kintrel_core::log::Durability;
use kintrel_core::proto::{self, FileContent, FileWrite, Post, Posted, ReadFile, SessionStart, SessionStarted, code};
use rusqlite::Connection;

const CART: &str = "use std::fmt;\n\nimpl Cart {\n    fn price(&self) -> u64 { 1 }\n\n    fn tax(&self) -> u64 { 2 }\n}\n";

async fn board() -> (PathBuf, PathBuf) {
    let repo = std::env::temp_dir().join(format!("kt-post-{}", std::process::id()));
    std::fs::remove_dir_all(&repo).ok();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/cart.rs"), CART).unwrap();
    kintrel_core::board::init(&repo).unwrap();
    let opts = Options { repo: repo.clone(), durability: Durability::Normal, promote_every: None };
    tokio::spawn(async move { daemon::run(opts).await.unwrap() });
    let sock = daemon::socket_path(&repo).unwrap();
    for _ in 0..200 {
        if Client::connect(&sock).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (repo, sock)
}

async fn session(sock: &std::path::Path, agent: &str) -> (Client, String) {
    let mut c = Client::connect(sock).await.unwrap();
    let s: SessionStarted =
        c.call(proto::SESSION_START, &SessionStart { agent: agent.into(), rank_hint: None, base: None }).await.unwrap();
    (c, s.token)
}

fn write(path: &str, content: Option<&str>, base: Option<Hash>) -> FileWrite {
    FileWrite { path: path.into(), content: content.map(Into::into), base }
}

async fn post(c: &mut Client, token: &str, files: Vec<FileWrite>) -> Result<Posted, CallError> {
    c.call(proto::POST, &Post { token: token.into(), files, summary: None }).await
}

async fn read(c: &mut Client, token: &str, path: &str) -> Result<FileContent, CallError> {
    c.call(proto::READ_FILE, &ReadFile { token: token.into(), path: path.into(), at: None }).await
}

fn code_of(r: Result<impl std::fmt::Debug, CallError>) -> i64 {
    match r {
        Err(CallError::Rpc(e)) => e.code,
        other => panic!("expected an rpc error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn post_path() {
    let (repo, sock) = board().await;
    let (mut a, ta) = session(&sock, "agent-a").await;
    let (mut b, tb) = session(&sock, "agent-b").await;

    let seen = read(&mut a, &ta, "src/cart.rs").await.unwrap();
    assert_eq!(seen.content, CART);

    // An edit to one method has a one-atom footprint, and the author sees its own write.
    let edited = CART.replace("{ 1 }", "{ 10 }");
    let p = post(&mut a, &ta, vec![write("src/cart.rs", Some(&edited), Some(seen.hash))]).await.unwrap();
    assert_eq!(p.atoms, ["src/cart.rs::impl Cart::fn price"]);
    assert_eq!(read(&mut a, &ta, "src/cart.rs").await.unwrap().content, edited);
    // Another session still sees its own base.
    assert_eq!(read(&mut b, &tb, "src/cart.rs").await.unwrap().content, CART);

    // A stale base is rejected with the current content, so the caller can retry at once.
    match post(&mut a, &ta, vec![write("src/cart.rs", Some("x"), Some(seen.hash))]).await {
        Err(CallError::Rpc(e)) => {
            assert_eq!(e.code, code::STALE_BASE);
            assert_eq!(e.data.unwrap()["current"], edited);
        }
        other => panic!("expected StaleBase, got {other:?}"),
    }

    // Create, then delete; deletes and no-ops are validated.
    let created = post(&mut a, &ta, vec![write("src/new.py", Some("def f():\n    pass\n"), Some(Hash::default()))]).await.unwrap();
    assert_eq!(created.atoms, ["src/new.py", "src/new.py::def f"]);
    post(&mut a, &ta, vec![write("src/new.py", None, None)]).await.unwrap();
    assert_eq!(code_of(read(&mut a, &ta, "src/new.py").await), code::NOT_FOUND);
    assert_eq!(code_of(post(&mut a, &ta, vec![write("src/new.py", None, None)]).await), code::INVALID_PARAMS);
    assert_eq!(code_of(post(&mut a, &ta, vec![write("src/cart.rs", Some(&edited), None)]).await), code::INVALID_PARAMS);
    // Writing under a file, or onto a directory, is rejected; so is an unsafe path.
    assert_eq!(code_of(post(&mut a, &ta, vec![write("src/cart.rs/x", Some("x"), None)]).await), code::INVALID_PARAMS);
    assert_eq!(code_of(post(&mut a, &ta, vec![write("src", Some("x"), None)]).await), code::INVALID_PARAMS);
    assert_eq!(code_of(post(&mut a, &ta, vec![write(".git/config", Some("x"), None)]).await), code::INVALID_PARAMS);
    // Paths must stay a tree within one post and against the session's own writes.
    let both = vec![write("dir", Some("x"), None), write("dir/file", Some("y"), None)];
    assert_eq!(code_of(post(&mut b, &tb, both).await), code::INVALID_PARAMS);
    post(&mut b, &tb, vec![write("own/file", Some("y"), None)]).await.unwrap();
    assert_eq!(code_of(post(&mut b, &tb, vec![write("own", Some("x"), None)]).await), code::INVALID_PARAMS);
    assert_eq!(code_of(post(&mut b, &tb, vec![write("own/file/deeper", Some("x"), None)]).await), code::INVALID_PARAMS);
    // Deleting the file in the same post frees the name.
    post(&mut b, &tb, vec![write("own/file", None, None), write("own", Some("x"), None)]).await.unwrap();
    assert_eq!(code_of(post(&mut b, "bogus", vec![write("a", Some("x"), None)]).await), code::UNAUTHORIZED);

    // The log holds three notes from session a, in seq order, with footprints and reads.
    let db = Connection::open(kintrel_core::board::log_db(&repo)).unwrap();
    let seqs: Vec<(u64, u64)> = db
        .prepare("SELECT session, seq FROM notes ORDER BY gseq")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(seqs, [(1, 1), (1, 2), (1, 3), (2, 1), (2, 2)]);
    let first: Vec<u8> = db.query_row("SELECT id FROM notes WHERE gseq = 1", [], |r| r.get(0)).unwrap();
    let reads: i64 = db.query_row("SELECT COUNT(*) FROM note_reads WHERE note = ?1", [&first], |r| r.get(0)).unwrap();
    assert_eq!(reads, 5, "the first note records every atom of the file it read");
    let writes: Vec<String> = db
        .prepare("SELECT atom FROM note_writes WHERE note = ?1")
        .unwrap()
        .query_map([&first], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(writes, ["src/cart.rs::impl Cart::fn price"]);
    let note: Vec<u8> = db.query_row("SELECT body FROM notes WHERE gseq = 1", [], |r| r.get(0)).unwrap();
    let note: kintrel_core::note::Note = kintrel_core::encode::decode(&note).unwrap();
    assert_eq!(note.parents, []);
    assert_eq!(note.reads.len(), 5);
    let second: Vec<u8> = db.query_row("SELECT body FROM notes WHERE gseq = 2", [], |r| r.get(0)).unwrap();
    let second: kintrel_core::note::Note = kintrel_core::encode::decode(&second).unwrap();
    assert_eq!(second.parents, [note.id()], "each note depends on the author's previous one");
    std::fs::remove_dir_all(kintrel_core::board::runtime_dir(&repo).unwrap()).ok();
    std::fs::remove_dir_all(&repo).ok();
}
