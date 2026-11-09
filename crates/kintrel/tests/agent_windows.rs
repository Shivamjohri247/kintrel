//! `kintrel agent` on Windows: in its AppContainer an agent writes its view and reaches the
//! daemon through its named pipe, but cannot read a person key or write the board.
#![cfg(windows)]

use std::time::Duration;

use kintrel::daemon::{self, Options};
use kintrel::rpc::Client;
use kintrel_ci::sandbox::{AgentPolicy, run_agent};
use kintrel_core::board;
use kintrel_core::log::Durability;

#[tokio::test(flavor = "multi_thread")]
async fn agents_work_in_their_view_but_cannot_act_as_the_person() {
    let repo = std::env::temp_dir().join(format!("kt-agent-win-{}", std::process::id()));
    std::fs::remove_dir_all(&repo).ok();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    board::init(&repo).unwrap();
    let opts = Options { repo: repo.clone(), durability: Durability::Normal, promote_every: None };
    tokio::spawn(async move { daemon::run(opts).await.unwrap() });
    let endpoint = daemon::socket_path(&repo).unwrap();
    while Client::connect(&endpoint).await.is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let runtime = board::runtime_dir(&repo).unwrap();
    let view = runtime.join("views").join("1");
    let config = runtime.join("agent-claude");
    let tmp = config.join("tmp");
    for d in [&view, &tmp] {
        std::fs::create_dir_all(d).unwrap();
    }
    // The CLI from inside the view: a test binary is not kintrel.exe, whose directory
    // `kintrel agent` would grant.
    let kintrel = view.join("kintrel.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_kintrel"), &kintrel).unwrap();
    let key = board::person_key(&repo).unwrap();
    let data = board::data_dir(&repo);
    let p = |p: &std::path::Path| p.display().to_string();
    let script = format!(
        "echo ok> {v}\\ok.txt & type {k} > {v}\\key.txt 2>&1 & echo x> {b}\\x.txt & {kin} --version > {v}\\version.txt 2>&1 & {kin} --repo {r} status > {v}\\status.txt 2>&1",
        v = p(&view),
        k = p(&key),
        b = p(&data),
        kin = p(&kintrel),
        r = p(&repo),
    );
    let cmd = std::path::PathBuf::from(std::env::var("SystemRoot").unwrap()).join("System32").join("cmd.exe");
    let argv = [cmd.into_os_string(), "/D".into(), "/C".into(), script.into()];
    let ours = [config.clone(), tmp.clone()];
    let keys = board::keys_dir().unwrap();
    let policy = AgentPolicy { view: &view, ours: &ours, extra: &[], board: &data, keys: &keys, socket: &endpoint };
    let env = [("TEMP".into(), tmp.clone().into_os_string()), ("TMP".into(), tmp.into_os_string())];
    run_agent(&argv, &policy, &env, &view).unwrap();

    let read = |name: &str| std::fs::read_to_string(view.join(name)).unwrap_or_default();
    assert_eq!(read("ok.txt").trim(), "ok", "the agent writes its view");
    let secret = std::fs::read_to_string(&key).unwrap();
    assert!(!read("key.txt").contains(secret.trim()), "the person key leaked");
    assert!(!data.join("x.txt").exists(), "the agent wrote the board");
    let acl = std::process::Command::new("icacls").arg(&kintrel).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    assert!(
        read("status.txt").contains("promoted"),
        "the daemon is reachable: {:?}\nversion: {:?}\nkintrel.exe ACL: {acl}",
        read("status.txt"),
        read("version.txt")
    );

    std::fs::remove_dir_all(&runtime).ok();
    std::fs::remove_dir_all(&repo).ok();
}
