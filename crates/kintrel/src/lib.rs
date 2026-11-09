//! The `kintrel` binary's library half: daemon, RPC framing and client. Shared with `kintrel-bench`.

pub mod ci_worker;
pub mod daemon;
pub mod git;
pub mod ipc;
pub mod mcp;
pub mod notices;
pub mod rpc;
pub mod view;

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};

use crate::rpc::Client;

/// Connects to the board's daemon, starting one in the background if none is running.
pub async fn connect(repo: &Path) -> anyhow::Result<Client> {
    let sock = daemon::socket_path(repo)?;
    if let Ok(c) = Client::connect(&sock).await {
        return Ok(c);
    }
    if !kintrel_core::board::state_db(repo).exists() {
        bail!("{} is not a kintrel board (run `kintrel init`)", repo.display());
    }
    // A sandboxed agent cannot write the board, so it cannot start a daemon either.
    let log = std::fs::File::create(kintrel_core::board::data_dir(repo).join("daemon.log"))
        .context("no daemon is running for this board, and this process cannot start one (a sandboxed agent cannot): run any kintrel command outside the sandbox first")?;
    let mut daemon = std::process::Command::new(std::env::current_exe()?);
    daemon
        .arg("--repo")
        .arg(std::fs::canonicalize(repo)?) // absolute, so `ps` shows which board a daemon serves
        .args(["daemon", "run"])
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // Detached from the caller's terminal and signals, so it outlives the command that started it.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut daemon, 0);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut daemon, 0x0000_0008 | 0x0000_0200); // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
    daemon.spawn().context("starting the daemon")?;
    for _ in 0..250 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Ok(c) = Client::connect(&sock).await {
            return Ok(c);
        }
    }
    bail!("the daemon did not start; see {}", kintrel_core::board::data_dir(repo).join("daemon.log").display())
}
