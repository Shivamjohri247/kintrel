//! How checks run (SPEC §14): in the platform shell, and by default in an OS sandbox, because
//! checks execute repository code that agents wrote. One policy, one backend per OS:
//!
//! | OS | Backend | Network off | Writes | Secrets hidden |
//! |---|---|---|---|---|
//! | macOS | `sandbox-exec` (Seatbelt) | outbound denied except loopback | checkout, temp, `writable` | `~/.ssh`, `~/.aws`, … |
//! | Linux | `bwrap` (bubblewrap) | new network namespace (loopback only) | same; `/tmp` is fresh | same (tmpfs or `/dev/null` over them) |
//! | Windows | AppContainer (`rappct`) | no network capability (loopback too) | checkout and `writable` | the whole profile, except `readable` |
//!
//! On Windows an AppContainer cannot read the user's profile at all, which hides secrets but
//! also toolchains installed there (`~/.rustup`, a user Python): list those under `readable`.
//! Each check runs as its own container (created for the run, deleted after), granted only its
//! own checkout, so it cannot touch another check's. Shared `writable` and `readable`
//! directories are granted to a stable capability, `kintrelChecks`, that every check's
//! container carries: an inheritable entry added to each one's ACL by `icacls`, once, which is
//! a lasting change to those directories.
//!
//! On every OS, a `writable` or `readable` entry that is, holds, or lies inside a secret path
//! (below) is refused: `readable = ["~"]` must not expose `~/.ssh`.
//!
//! Whatever `network` says, a check never connects to a Unix socket outside its checkout (on
//! macOS: except the DNS resolver's). The Kintrel daemon's socket is one: a check that reached
//! it could post notes or approve a config. So are Docker's and an SSH agent's. On macOS the
//! keychain, pasteboard and app-launch services are refused too.
// ponytail: macOS starts from `(allow default)` and denies what matters; a `(deny default)`
// profile is stronger but needs a long, fragile allowlist of system services.
//!
//! A backend that is missing is an error, never a silent unsandboxed run: a check that needs
//! no sandbox says `sandbox = false` in `kintrel.toml`.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// What a sandboxed check may do beyond reading files.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// Allow network beyond loopback.
    pub network: bool,
    /// Directories the check may write besides its checkout and the temp directories.
    pub writable: Vec<PathBuf>,
    /// Directories it may read where reads are otherwise refused (Windows: the user profile).
    pub readable: Vec<PathBuf>,
    /// Kintrel's own directories (the runtime directory with the daemon's key and every view and
    /// checkout; the board's `.kintrel`): no `writable` or `readable` entry may reach them.
    pub protected: Vec<PathBuf>,
    /// Directories the check never reads, like the secret paths: the board's `.kintrel`, which
    /// holds the person key. (Windows: unreadable to an AppContainer unless granted, and grants
    /// there are refused through `protected`.)
    pub hidden: Vec<PathBuf>,
}

/// A started check: its merged output, and a wait that kills it (and everything it started)
/// at the timeout.
pub struct Running {
    pub output: Box<dyn Read + Send>,
    wait: Box<dyn FnOnce(Duration) -> Option<bool> + Send>,
}

impl Running {
    /// `Some(success)`, or `None` if it was killed at the timeout.
    pub fn wait(self, timeout: Duration) -> Option<bool> {
        (self.wait)(timeout)
    }
}

/// Starts `cmd` in the platform shell at `dir`, with exactly `env`, in the sandbox `policy`
/// (or unconfined with `None`).
pub fn start(cmd: &str, dir: Option<&Path>, policy: Option<&Policy>, env: &[(String, OsString)]) -> Result<Running, String> {
    // Grant paths are resolved once, here, and used as resolved: never re-resolved later, where
    // a concurrent check could have swapped a directory for a symlink in between.
    let resolved = policy.map(|p| refuse_secrets(p, dir)).transpose()?;
    let policy = resolved.as_ref();
    #[cfg(windows)]
    if let Some(policy) = policy {
        return appcontainer::start(cmd, dir, policy, env);
    }
    let mut c = match policy {
        Some(policy) => command(cmd, dir, policy)?,
        None => plain_shell(cmd),
    };
    // stdout and stderr share one pipe at the process level, so the sandbox's own errors (bwrap
    // failing to set up, say) reach the output, not just the check's (`2>&1` in the shell).
    let (output, writer) = std::io::pipe().map_err(|e| format!("pipe: {e}"))?;
    let stderr = writer.try_clone().map_err(|e| format!("pipe: {e}"))?;
    c.env_clear().envs(env.iter().map(|(k, v)| (k, v))).stdin(Stdio::null()).stdout(writer).stderr(stderr);
    if let Some(dir) = dir {
        c.current_dir(dir);
    }
    // Its own process group, so a timeout kills everything the check started: a surviving
    // grandchild would hold the output pipe open (and keep running).
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut c, 0);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut c, 0x0000_0200); // CREATE_NEW_PROCESS_GROUP
    let mut child = c.spawn().map_err(|e| format!("could not start: {e}"))?;
    drop(c); // its copies of the pipe's write end, or the output never ends
    let output = Box::new(output);
    let wait = move |timeout: Duration| {
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(s)) => return Some(s.success()),
                Ok(None) if start.elapsed() > timeout => {
                    kill_tree(child.id());
                    child.kill().ok();
                    child.wait().ok();
                    return None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => return Some(false),
            }
        }
    };
    Ok(Running { output, wait: Box::new(wait) })
}

/// The platform shell: `/bin/sh -c` on Unix; on Windows `cmd /D /S /C "cmd"`, passed verbatim
/// (Rust's usual argument quoting follows MSVC rules, which cmd does not).
fn plain_shell(cmd: &str) -> Command {
    #[cfg(unix)]
    {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(cmd);
        c
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd.exe");
        c.raw_arg(format!("/D /S /C \"{cmd}\""));
        c
    }
}

/// Validates and resolves a policy's grants. A `writable` or `readable` path that is, holds, or
/// lies inside a secret path or one of Kintrel's own directories is refused, whether or not it
/// exists yet, as is any path with `.` or `..` in it. Paths inside the check's own checkout
/// (`own`) are dropped: the checkout is writable already, so they need no grant (and no grant
/// means nothing a concurrent check could redirect). The rest come back resolved.
fn refuse_secrets(policy: &Policy, own: Option<&Path>) -> Result<Policy, String> {
    let own = own.map(|o| fold(&resolve(o)));
    let home = kintrel_core::os::home_dir();
    let guarded: Vec<PathBuf> = home
        .iter()
        .flat_map(|h| SECRETS.iter().map(move |s| h.join(s)))
        .chain(policy.protected.iter().cloned())
        .map(|g| fold(&resolve(&g)))
        .collect();
    let check = |list: &[PathBuf]| -> Result<Vec<PathBuf>, String> {
        let mut out = vec![];
        for p in list {
            if p.components().any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir)) {
                return Err(format!("{}: `.` and `..` are not allowed in writable or readable paths", p.display()));
            }
            let resolved = resolve(p);
            let folded = fold(&resolved);
            if own.as_ref().is_some_and(|o| folded.starts_with(o)) {
                continue;
            }
            if let Some(g) = guarded.iter().find(|g| g.starts_with(&folded) || folded.starts_with(g)) {
                return Err(format!("{} would expose {}; list a narrower directory", p.display(), g.display()));
            }
            // Only existing, fully resolved directories are granted: a missing tail could later be
            // created as a symlink by a check. (Residual: a grant inside another writable grant
            // could still be swapped between this check and the bind; no-follow handles would
            // close it but need unsafe OS calls, which this workspace forbids.)
            let resolved = std::fs::canonicalize(p).map_err(|_| format!("{} does not exist; create it before granting it", p.display()))?;
            out.push(resolved);
        }
        Ok(out)
    };
    Ok(Policy {
        network: policy.network,
        writable: check(&policy.writable)?,
        readable: check(&policy.readable)?,
        protected: vec![],
        hidden: policy.hidden.clone(),
    })
}

/// A path resolved as far as it exists: the nearest existing ancestor canonicalized (symlinks
/// resolved), the rest appended, Windows' `\\?\` prefix dropped. (Callers refuse `..` first.)
fn resolve(p: &Path) -> PathBuf {
    let mut rest = vec![];
    let mut base = p.to_owned();
    let canon = loop {
        if let Ok(c) = std::fs::canonicalize(&base) {
            break c;
        }
        match (base.file_name().map(ToOwned::to_owned), base.parent().map(Path::to_owned)) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                base = parent;
            }
            _ => break base,
        }
    };
    let mut out = canon;
    for name in rest.into_iter().rev() {
        out.push(name);
    }
    let text = out.to_string_lossy().into_owned();
    PathBuf::from(text.strip_prefix(r"\\?\").map(str::to_owned).unwrap_or(text))
}

/// The form paths are compared in: lower case where the file system ignores case by default
/// (macOS, Windows).
fn fold(p: &Path) -> PathBuf {
    if cfg!(any(windows, target_os = "macos")) { PathBuf::from(p.to_string_lossy().to_lowercase()) } else { p.to_owned() }
}

/// Kills a check and everything it started.
fn kill_tree(pid: u32) {
    #[cfg(unix)]
    Command::new("/bin/kill").args(["-KILL", "--", &format!("-{pid}")]).status().ok();
    #[cfg(windows)]
    Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]).status().ok();
}

/// Directories under the home directory that checks never see: their output reaches agents.
pub const SECRETS: &[&str] = &[
    ".ssh",
    ".aws",
    ".azure",
    ".gnupg",
    ".config/gh",
    ".config/gcloud",
    ".netrc",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".docker",
    ".kube",
    "Library/Keychains",
];

/// Sockets that grant far more than a check should have (Linux; macOS refuses every outside socket).
#[cfg(target_os = "linux")]
const SOCKETS: &[&str] = &["/var/run/docker.sock", "/run/docker.sock", "/run/podman/podman.sock"];

/// The sockets that exist, resolved and once each: where `/var/run` links to `/run` (GitHub's
/// runners), bubblewrap cannot mount over `/var/run/docker.sock` inside its read-only root.
#[cfg(target_os = "linux")]
fn sockets() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = SOCKETS.iter().filter_map(|s| std::fs::canonicalize(s).ok()).collect();
    v.sort();
    v.dedup();
    v
}

/// `sh -c cmd` inside the sandbox, with `dir` (the checkout) writable. `Err` names what is
/// missing on this system. (Windows starts differently: see `appcontainer`.)
#[cfg_attr(windows, allow(dead_code))]
fn command(cmd: &str, dir: Option<&Path>, policy: &Policy) -> Result<Command, String> {
    let home = kintrel_core::os::home_dir();
    let secrets: Vec<PathBuf> = home
        .iter()
        .flat_map(|h| SECRETS.iter().map(move |s| h.join(s)))
        .chain(policy.hidden.iter().cloned())
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .collect();
    // The checkout and temp directories are Kintrel's; policy paths arrive resolved already.
    let mut writable: Vec<PathBuf> =
        dir.into_iter().map(Path::to_owned).chain(temp_dirs()).map(|p| std::fs::canonicalize(&p).unwrap_or(p)).collect();
    writable.extend(policy.writable.iter().cloned());
    backend(cmd, dir, policy, &writable, &secrets)
}

/// Where an agent may write and what it must never touch.
pub struct AgentPolicy<'a> {
    /// The agent's view; it starts there.
    pub view: &'a Path,
    /// Kintrel's own writable directories for the agent (its Claude Code config).
    pub ours: &'a [PathBuf],
    /// More writable directories, validated like a check's `writable`.
    pub extra: &'a [PathBuf],
    /// The board's `.kintrel`: read-only (views read it).
    pub board: &'a Path,
    /// Every board's person key: unreadable.
    pub keys: &'a Path,
    /// The daemon's socket: the one Unix socket the agent may connect to (besides DNS on macOS).
    pub socket: &'a Path,
}

/// An agent's command (`argv`), unable to act as the person (SPEC §13, local form). It writes
/// only its view, the temp directories, `ours` and `extra`; it never writes the board, never
/// reads a person key, and cannot reach what runs code outside the sandbox: other Unix sockets
/// (a tmux server, Docker, the user's D-Bus and systemd), LaunchServices and Apple events on
/// macOS, the person's terminal (`TIOCSTI`). Reads, the network and the keychain stay open:
/// an agent needs its tools and its model.
// ponytail: macOS and Linux; Windows needs an AppContainer with the profile granted.
pub fn agent(argv: &[OsString], p: &AgentPolicy) -> Result<Command, String> {
    let program = argv.first().ok_or("no agent command given")?;
    if program.to_string_lossy().starts_with('-') {
        return Err("the agent command cannot start with `-`".into());
    }
    let protected = vec![p.board.to_owned(), p.keys.to_owned()];
    let extra = refuse_secrets(&Policy { writable: p.extra.to_vec(), protected, ..Policy::default() }, Some(p.view))?.writable;
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    let writable: Vec<PathBuf> = std::iter::once(p.view.to_owned()).chain(p.ours.iter().cloned()).map(|p| canon(&p)).chain(extra).collect();
    let resolved = AgentPolicy { view: &canon(p.view), board: &canon(p.board), keys: &canon(p.keys), socket: &canon(p.socket), ..*p };
    let mut c = agent_backend(argv, &resolved, &writable)?;
    c.current_dir(p.view);
    Ok(c)
}

#[cfg(target_os = "macos")]
fn agent_backend(argv: &[OsString], p: &AgentPolicy, writable: &[PathBuf]) -> Result<Command, String> {
    let lit = |p: &Path| format!("\"{}\"", p.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\""));
    let mut profile = String::from("(version 1)(allow default)(deny file-write*)(allow file-write* (subpath \"/dev\")");
    for w in writable.iter().cloned().chain(temp_dirs().into_iter().map(|t| std::fs::canonicalize(&t).unwrap_or(t))) {
        profile.push_str(&format!(" (subpath {})", lit(&w)));
    }
    // Later rules win: the board stays read-only even inside a writable directory.
    profile.push_str(&format!(")(deny file-write* (subpath {}))(deny file-read* file-write* (subpath {}))", lit(p.board), lit(p.keys)));
    // Nothing that runs code outside the sandbox: other sockets, app launches, Apple events,
    // keystrokes into the person's terminal. (`launchctl submit` is refused by Seatbelt itself.)
    profile.push_str(&format!(
        "(deny network-outbound (remote unix-socket))\
         (allow network-outbound (remote unix-socket (path-literal \"/private/var/run/mDNSResponder\")))\
         (allow network-outbound (remote unix-socket (path-literal {})))\
         (allow network-outbound (remote unix-socket (subpath {})))\
         (deny mach-lookup (global-name \"com.apple.coreservices.launchservicesd\"))(deny appleevent-send)\
         (deny file-ioctl (ioctl-command TIOCSTI))",
        lit(p.socket),
        lit(p.view)
    ));
    let mut c = Command::new("/usr/bin/sandbox-exec");
    c.arg("-p").arg(profile).args(argv);
    Ok(c)
}

#[cfg(target_os = "linux")]
fn agent_backend(argv: &[OsString], p: &AgentPolicy, writable: &[PathBuf]) -> Result<Command, String> {
    use std::os::unix::fs::MetadataExt;
    let bwrap = ["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap"]
        .iter()
        .map(Path::new)
        .find(|p| p.exists())
        .ok_or("no sandbox on this system: install bubblewrap (bwrap)")?;
    let mut c = Command::new(bwrap);
    // A new session: the agent cannot push keystrokes into the person's terminal (TIOCSTI). A
    // private /tmp: no tmux or X11 sockets there.
    c.args(["--ro-bind", "/", "/", "--dev", "/dev", "--unshare-pid", "--proc", "/proc", "--new-session", "--die-with-parent"]);
    c.args(["--tmpfs", "/tmp"]);
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

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn agent_backend(_argv: &[OsString], _p: &AgentPolicy, _writable: &[PathBuf]) -> Result<Command, String> {
    Err("sandboxed agents are not available on this OS yet".into())
}

fn temp_dirs() -> Vec<PathBuf> {
    let mut v = vec![std::env::temp_dir(), PathBuf::from("/tmp")];
    v.dedup();
    v
}

#[cfg(target_os = "macos")]
fn backend(cmd: &str, dir: Option<&Path>, policy: &Policy, writable: &[PathBuf], secrets: &[PathBuf]) -> Result<Command, String> {
    // Seatbelt string literals: escape backslashes and quotes.
    let lit = |p: &Path| format!("\"{}\"", p.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\""));
    let mut profile = String::from("(version 1)(allow default)");
    // Outbound: loopback (test servers), the DNS resolver, and sockets inside the checkout.
    profile.push_str(
        "(deny network-outbound)(allow network-outbound (remote ip \"localhost:*\"))\
         (allow network-outbound (remote unix-socket (path-literal \"/private/var/run/mDNSResponder\")))",
    );
    if let Some(dir) = dir {
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_owned());
        profile.push_str(&format!("(allow network-outbound (remote unix-socket (subpath {})))", lit(&dir)));
    }
    if policy.network {
        profile.push_str("(allow network-outbound (remote ip \"*:*\"))");
    }
    profile.push_str(
        "(deny mach-lookup (global-name \"com.apple.SecurityServer\") (global-name \"com.apple.securityd\") \
         (global-name \"com.apple.pasteboard.1\") (global-name \"com.apple.coreservices.launchservicesd\"))",
    );
    profile.push_str("(deny file-write*)(allow file-write* (subpath \"/dev\")");
    for p in writable {
        profile.push_str(&format!(" (subpath {})", lit(p)));
    }
    profile.push(')');
    for p in secrets {
        profile.push_str(&format!("(deny file-read* (subpath {}))", lit(p)));
    }
    let mut c = Command::new("/usr/bin/sandbox-exec");
    c.arg("-p").arg(profile).args(["/bin/sh", "-c", cmd]);
    Ok(c)
}

#[cfg(target_os = "linux")]
fn backend(cmd: &str, dir: Option<&Path>, policy: &Policy, writable: &[PathBuf], secrets: &[PathBuf]) -> Result<Command, String> {
    // ponytail: written against bubblewrap's documented flags; not run on this macOS dev machine.
    let bwrap = ["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap"]
        .iter()
        .map(Path::new)
        .find(|p| p.exists())
        .ok_or("no sandbox on this system: install bubblewrap (bwrap), or set sandbox = false for this check")?;
    let mut c = Command::new(bwrap);
    // A read-only root, own PID namespace and session (no signalling or TIOCSTI into the
    // caller's terminal), and a fresh /tmp (agent sockets live there).
    c.args(["--ro-bind", "/", "/", "--dev", "/dev", "--unshare-pid", "--proc", "/proc", "--new-session", "--die-with-parent"]);
    c.args(["--tmpfs", "/tmp"]);
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

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn backend(_cmd: &str, _dir: Option<&Path>, _policy: &Policy, _writable: &[PathBuf], _secrets: &[PathBuf]) -> Result<Command, String> {
    Err("no sandbox backend for this OS; set sandbox = false for this check".into())
}

/// Windows: the check runs in the AppContainer `kintrel.checks`, inside a job object that
/// kills its whole process tree when the wait ends.
// ponytail: compile-checked from macOS, not yet run on Windows (no Windows machine here).
#[cfg(windows)]
mod appcontainer {
    use super::{Policy, Running};
    use rappct::launch::{JobLimits, LaunchOptions, StdioConfig, launch_in_container_with_io};
    use rappct::{AppContainerProfile, KnownCapability, SecurityCapabilitiesBuilder};
    use std::collections::HashSet;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    /// Every check's container carries this capability; shared directories are granted to it.
    const CAPABILITY: &str = "kintrelChecks";

    pub fn start(cmd: &str, dir: Option<&Path>, policy: &Policy, env: &[(String, OsString)]) -> Result<Running, String> {
        let e = |what: &str, err: &dyn std::fmt::Display| format!("AppContainer {what}: {err}");
        // One container per checkout (its directory name is unique per run), so a check reaches
        // only its own checkout; deleted when the check ends.
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tag = dir.and_then(Path::file_name).map_or_else(
            || format!("tools-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
            |n| n.to_string_lossy().into_owned(),
        );
        let name = format!("kintrel.check.{tag}");
        let profile = AppContainerProfile::ensure(&name, "Kintrel check", Some("One Kintrel check run")).map_err(|x| e("profile", &x))?;
        if let Some(dir) = dir {
            grant(profile.sid.as_string(), dir, "M", false)?;
        }
        let shared = rappct::capability::derive_named_capability_sids(&[CAPABILITY]).map_err(|x| e("capability", &x))?;
        let shared = &shared.first().ok_or("AppContainer capability: no SID")?.sid_sddl;
        for p in &policy.writable {
            grant(shared, p, "M", true)?;
        }
        for p in &policy.readable {
            grant(shared, p, "RX", true)?;
        }
        let caps: &[KnownCapability] =
            if policy.network { &[KnownCapability::InternetClient, KnownCapability::PrivateNetworkClientServer] } else { &[] };
        let sec = SecurityCapabilitiesBuilder::new(&profile.sid)
            .with_known(caps)
            .with_named(&[CAPABILITY])
            .build()
            .map_err(|x| e("capabilities", &x))?;
        let comspec = std::env::var_os("COMSPEC").map_or_else(|| PathBuf::from(r"C:\Windows\System32\cmd.exe"), PathBuf::from);
        // Exactly the allowlist (which has the Windows essentials): never merge the parent's.
        let env = env.iter().map(|(k, v)| (OsString::from(k), v.clone())).collect();
        let opts = LaunchOptions {
            exe: comspec,
            cmdline: Some(format!(" /D /S /C \"{cmd}\"")),
            cwd: dir.map(Path::to_owned),
            env: Some(env),
            stdio: StdioConfig::Pipe,
            join_job: Some(JobLimits { kill_on_job_close: true, ..Default::default() }),
            ..Default::default()
        };
        let mut io = launch_in_container_with_io(&sec, &opts).map_err(|x| e("launch", &x))?;
        let output: Box<dyn std::io::Read + Send> = match io.stdout.take() {
            Some(f) => Box::new(f),
            None => Box::new(std::io::empty()),
        };
        // stderr is merged into stdout by the command line; drop the unused pipe's handle.
        drop(io.stderr.take());
        // Waiting consumes the launch; at the timeout that drops the job, killing the tree. The
        // container goes with it.
        let wait = move |timeout: Duration| {
            let r = match io.wait(Some(timeout)) {
                Ok(code) => Some(code == 0),
                Err(rappct::AcError::LaunchFailed { stage: "wait", .. }) => None,
                Err(_) => Some(false),
            };
            profile.delete().ok();
            r
        };
        Ok(Running { output, wait: Box::new(wait) })
    }

    /// Adds an inheritable ACE for `sid` to `dir` with `icacls`, propagated to what is already
    /// inside. With `once`, only the first time per daemon (stable SIDs).
    // ponytail: a checkout is granted after it is populated, so icacls walks it; granting the
    // empty directory before cloning into it would skip the walk.
    fn grant(sid: &str, dir: &Path, rights: &str, once: bool) -> Result<(), String> {
        static DONE: OnceLock<Mutex<HashSet<(PathBuf, String)>>> = OnceLock::new();
        let done = DONE.get_or_init(Default::default);
        let key = (dir.to_owned(), format!("{sid}:{rights}"));
        if (once && done.lock().unwrap().contains(&key)) || !dir.exists() {
            return Ok(());
        }
        let status = Command::new("icacls")
            .arg(dir)
            .args(["/grant", &format!("*{sid}:(OI)(CI){rights}"), "/T", "/C", "/Q"])
            .status()
            .map_err(|e| format!("icacls: {e}"))?;
        if !status.success() {
            return Err(format!("icacls could not grant {rights} on {} to {sid}", dir.display()));
        }
        done.lock().unwrap().insert(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refuse_secrets_t(p: &Policy) -> Result<Policy, String> {
        refuse_secrets(p, None)
    }

    #[test]
    fn paths_around_secrets_are_refused() {
        let Some(home) = kintrel_core::os::home_dir() else { return };
        let policy = |p: PathBuf| Policy { writable: vec![p], ..Policy::default() };
        assert!(refuse_secrets_t(&policy(home.clone())).is_err(), "home holds ~/.ssh");
        assert!(refuse_secrets_t(&policy(home.join(".ssh/keys/not-yet"))).is_err(), "inside a secret, not existing yet");
        assert!(refuse_secrets_t(&Policy { readable: vec![home.join(".aws")], ..Policy::default() }).is_err());
        let cache = std::env::temp_dir().join(format!("kt-grant-{}", std::process::id()));
        assert!(refuse_secrets_t(&policy(cache.clone())).unwrap_err().contains("does not exist"), "missing paths are not granted");
        std::fs::create_dir_all(&cache).unwrap();
        assert!(refuse_secrets_t(&policy(cache.clone())).is_ok());
        std::fs::remove_dir_all(&cache).ok();
        if cfg!(any(windows, target_os = "macos")) {
            let upper = PathBuf::from(home.to_string_lossy().to_uppercase()).join(".SSH");
            assert!(refuse_secrets_t(&policy(upper)).is_err(), "case-insensitive file systems");
        }
        // Kintrel's own directories.
        let runtime = std::env::temp_dir().join("kt-protected-test/kintrel");
        let guarded = |p: PathBuf| Policy { readable: vec![p], protected: vec![runtime.clone()], ..Policy::default() };
        assert!(refuse_secrets_t(&guarded(runtime.join("abc/key"))).is_err(), "the daemon's key");
        assert!(refuse_secrets_t(&guarded(runtime.parent().unwrap().to_owned())).is_err(), "a directory holding it");
        let own = runtime.join("checks/1");
        let inside = refuse_secrets(&guarded(own.join("target")), Some(&own)).unwrap();
        assert!(inside.readable.is_empty(), "inside the check's own checkout: allowed, nothing to grant");
        assert!(refuse_secrets(&guarded(runtime.join("checks/2")), Some(&own)).is_err(), "another checkout");
        let escape = refuse_secrets(&guarded(own.join("x/../../2")), Some(&own));
        assert!(escape.unwrap_err().contains(".."), "`..` cannot climb out of the checkout");
    }
}
