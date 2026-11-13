//! Linux: bubblewrap (`bwrap`), a mount namespace built per run.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{AgentPolicy, Policy, temp_dirs};

/// Sockets that grant far more than a check should have (Linux; macOS refuses every outside socket).
const SOCKETS: &[&str] = &["/var/run/docker.sock", "/run/docker.sock", "/run/podman/podman.sock"];

/// The sockets that exist, resolved and once each: where `/var/run` links to `/run` (GitHub's
/// runners), bubblewrap cannot mount over `/var/run/docker.sock` inside its read-only root.
fn sockets() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = SOCKETS.iter().filter_map(|s| std::fs::canonicalize(s).ok()).collect();
    v.sort();
    v.dedup();
    v
}

/// `bwrap` with a read-only root, its own PID namespace and session (no signalling or TIOCSTI
/// into the caller's terminal), and a fresh /tmp (agent, tmux and X11 sockets live there).
/// `missing` ends the error when bubblewrap is not installed.
fn bwrap(missing: &str) -> Result<Command, String> {
    let bwrap = ["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap"]
        .iter()
        .map(Path::new)
        .find(|p| p.exists())
        .ok_or_else(|| format!("no sandbox on this system: install bubblewrap (bwrap){missing}"))?;
    let mut c = Command::new(bwrap);
    c.args(["--ro-bind", "/", "/", "--dev", "/dev", "--unshare-pid", "--proc", "/proc", "--new-session", "--die-with-parent"]);
    c.args(["--tmpfs", "/tmp"]);
    Ok(c)
}

pub(super) fn backend(cmd: &str, dir: Option<&Path>, policy: &Policy, writable: &[PathBuf], secrets: &[PathBuf]) -> Result<Command, String> {
    let mut c = bwrap(", or set sandbox = false for this check")?;
    // Kintrel's runtime directory holds the daemon's socket and other agents' views: hidden,
    // except this check's own checkout, bound below.
    if let Some(runtime) = dir.and_then(|d| d.ancestors().find(|a| a.file_name().is_some_and(|n| n == "kintrel"))) {
        c.arg("--tmpfs").arg(runtime);
    }
    // The system temp directories are the fresh tmpfs above; anything else writable, even under
    // /tmp, is bound from the host (mounts apply in order).
    let temps: Vec<PathBuf> = temp_dirs().into_iter().map(|p| std::fs::canonicalize(&p).unwrap_or(p)).collect();
    for p in writable.iter().filter(|p| p.exists() && !temps.contains(p)) {
        c.arg("--bind").arg(p).arg(p);
    }
    for p in secrets.iter().chain(&sockets()) {
        if p.is_dir() {
            c.arg("--tmpfs").arg(p);
        } else if p.exists() {
            c.arg("--ro-bind").arg("/dev/null").arg(p);
        }
    }
    if !policy.network {
        c.arg("--unshare-net");
    }
    c.args(["--", "/bin/sh", "-c", cmd]);
    Ok(c)
}

pub(super) fn agent_backend(argv: &[OsString], p: &AgentPolicy, writable: &[PathBuf]) -> Result<Command, String> {
    use std::os::unix::fs::MetadataExt;
    let mut c = bwrap("")?;
    for w in writable.iter().filter(|w| w.exists()) {
        c.arg("--bind").arg(w).arg(w);
    }
    // Mounts apply in order: the board read-only again, the keys gone.
    c.arg("--ro-bind").arg(p.board).arg(p.board);
    c.arg("--tmpfs").arg(p.keys);
    // The user's D-Bus and systemd (`systemd-run --user` would run code outside), and Docker.
    let uid = std::fs::metadata("/proc/self").map_err(|e| format!("/proc/self: {e}"))?.uid();
    for d in [PathBuf::from(format!("/run/user/{uid}")), PathBuf::from("/run/screen")] {
        if d.is_dir() {
            c.arg("--tmpfs").arg(&d);
        }
    }
    for s in sockets() {
        c.arg("--ro-bind").arg("/dev/null").arg(s);
    }
    // ponytail: bubblewrap cannot filter connect() by path, so Linux hides the usual places
    // (/tmp, /run/user, screen, Docker) and leaves sockets elsewhere reachable; abstract
    // sockets (an X11 display) share the network namespace the agent keeps for its model.
    // Landlock's scoped sockets (Linux 6.12) would close both.
    let _ = p.socket; // inside the runtime directory, reachable through the read-only root
    c.arg("--").args(argv);
    Ok(c)
}
