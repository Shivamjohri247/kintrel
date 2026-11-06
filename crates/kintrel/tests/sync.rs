//! `kintrel sync` (SPEC §11): commits people make in Git become notes, merged into head with
//! agents' work; Kintrel's own exports are skipped, and merging an export back into the branch
//! brings in only the person's side.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use kintrel::daemon::{self, Options};
use kintrel::rpc::Client;
use kintrel_core::log::Durability;
use kintrel_core::proto::{self, FileWrite, Post, Posted, SessionStart, SessionStarted, StatusInfo, Synced};
use kintrel_core::{board, state::State};

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

const BASE: &str = "def f():\n    return 1\n\n\ndef g():\n    return 2\n\n\ndef h():\n    return 3\n";

#[tokio::test(flavor = "multi_thread")]
async fn git_commits_merge_with_agents_work() {
    let repo = std::env::temp_dir().join(format!("kt-sync-{}", std::process::id()));
    fs::remove_dir_all(&repo).ok();
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("a.py"), BASE).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "start"]);
    board::init_with(&repo, |st| kintrel::git::import_head(st, &repo).map_err(|e| kintrel_core::Error::Invalid(e.to_string()))).unwrap();
    let opts = Options { repo: repo.clone(), durability: Durability::Normal, promote_every: None };
    tokio::spawn(async move { daemon::run(opts).await.unwrap() });
    let sock = daemon::socket_path(&repo).unwrap();
    while Client::connect(&sock).await.is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut c = Client::connect(&sock).await.unwrap();
    let agent: SessionStarted = c.call(proto::SESSION_START, &SessionStart { agent: "a".into(), rank_hint: None, base: None }).await.unwrap();
    let mut head_a = BASE.to_owned();
    let post = async |c: &mut Client, text: String| {
        let files = vec![FileWrite { path: "a.py".into(), content: Some(text), base: None }];
        let _: Posted = c.call(proto::POST, &Post { token: agent.token.clone(), files, summary: None }).await.unwrap();
    };
    let head = async |c: &mut Client| {
        let st: StatusInfo = c.call(proto::PROMOTE_NOW, &proto::PromoteNow::default()).await.unwrap();
        let read = proto::ReadFile { token: String::new(), path: "a.py".into(), at: st.head };
        let f: proto::FileContent = c.call(proto::READ_FILE, &read).await.unwrap();
        f.content
    };
    let sync = async |c: &mut Client| -> Synced { c.call(proto::SYNC, &proto::Sync::default()).await.unwrap() };

    // An agent changes f; a person commits a change to g, a binary file and a script.
    head_a = head_a.replace("return 1", "return 10");
    post(&mut c, head_a.clone()).await;
    fs::write(repo.join("a.py"), BASE.replace("return 2", "return 20")).unwrap();
    fs::write(repo.join("bin.dat"), [0u8, 255, 1]).unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "person: g and data"]);
    let s = sync(&mut c).await;
    assert_eq!((s.branch.as_str(), s.commits.len()), ("main", 1), "{s:?}");
    assert!(s.commits[0].lost.is_empty() && s.commits[0].summary == "person: g and data", "{s:?}");
    let merged = head(&mut c).await;
    assert!(merged.contains("return 10") && merged.contains("return 20"), "both changes in head: {merged}");
    assert!(sync(&mut c).await.commits.is_empty(), "up to date");
    let st = State::open_with_log(&board::state_db(&repo), &board::log_db(&repo)).unwrap();
    let at = st.get_ref(kintrel_core::state::HEAD).unwrap().unwrap();
    let (_, bin) = st.lookup(at, "bin.dat").unwrap().expect("binary file synced");
    assert_eq!(st.get_blob(bin).unwrap().unwrap(), [0u8, 255, 1]);

    // Export, and merge the export back into main with one more change of the person's (h)
    // made in the merge; meanwhile the agent changes f again. Only h comes in, and the agent's
    // newer f survives.
    let promoted = st.get_ref(kintrel_core::state::PROMOTED).unwrap().unwrap();
    kintrel::git::export(&st, &repo, promoted, kintrel::git::DEFAULT_BRANCH).unwrap().expect("a new commit");
    // Provenance: the agent's note is named; the note sync made from the person's commit is not.
    let msg = git(&repo, &["log", "-1", "--format=%B", kintrel::git::DEFAULT_BRANCH]);
    let notes: Vec<&str> = msg.lines().filter(|l| l.starts_with("Kintrel-Note: ")).collect();
    assert!(notes.len() == 1 && notes[0].ends_with(" a"), "{msg}");
    git(&repo, &["merge", "-q", "--no-ff", "--no-commit", kintrel::git::DEFAULT_BRANCH]);
    fs::write(repo.join("a.py"), fs::read_to_string(repo.join("a.py")).unwrap().replace("return 3", "return 30")).unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "merge kintrel, and h"]);
    head_a = merged.replace("return 10", "return 11");
    post(&mut c, head_a).await;
    let s = sync(&mut c).await;
    assert_eq!(s.commits.len(), 1, "the export itself is skipped: {s:?}");
    assert!(s.commits[0].lost.is_empty(), "{s:?}");
    let merged = head(&mut c).await;
    assert!(merged.contains("return 11") && merged.contains("return 20") && merged.contains("return 30"), "{merged}");

    // A person's commit that only quotes an export trailer is still theirs, and synced.
    fs::write(repo.join("notes.txt"), "x\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "quote\n\nKintrel-Root: 0000"]);
    assert_eq!(sync(&mut c).await.commits.len(), 1);
    // Clients cannot take the sync's name (its notes are left out of export trailers) or put
    // a newline in theirs (trailers are lines).
    for agent in ["git", "kintrel-tier3", "a\nKintrel-Root: x", ""] {
        let r: Result<SessionStarted, _> = c.call(proto::SESSION_START, &SessionStart { agent: agent.into(), rank_hint: None, base: None }).await;
        assert!(r.is_err(), "{agent:?}");
    }

    // The same function changed both ways is a conflict, reported, never a lost change.
    post(&mut c, merged.replace("return 20", "return 21")).await;
    fs::write(repo.join("a.py"), fs::read_to_string(repo.join("a.py")).unwrap().replace("return 20", "return 22")).unwrap();
    git(&repo, &["commit", "-qam", "person: g again"]);
    let s = sync(&mut c).await;
    assert_eq!(s.commits[0].lost.len(), 1, "{s:?}");
    assert!(head(&mut c).await.contains("return 21"), "head keeps the agent's version until someone resolves it");

    fs::remove_dir_all(board::runtime_dir(&repo).unwrap()).ok();
    fs::remove_dir_all(&repo).ok();
}
