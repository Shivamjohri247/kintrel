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

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
mod appcontainer;
#[cfg(target_os = "linux")]
mod bwrap;
#[cfg(target_os = "macos")]
mod seatbelt;

#[cfg(target_os = "linux")]
use bwrap::{agent_backend, backend};
#[cfg(target_os = "macos")]
use seatbelt::{agent_backend, backend};

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
    /// On Windows, the runtime directory, which holds the daemon pipe's name and key.
    pub socket: &'a Path,
}

/// The capability a board's agents carry in their AppContainers (Windows): that board's daemon
/// pipe admits it, and what the board's agents read is granted to it. One per board (named by
/// its runtime directory), so an agent reaches no other board's daemon or files.
#[cfg_attr(not(windows), allow(dead_code))]
fn agents_capability(runtime: &Path) -> String {
    format!("kintrelAgents.{}", runtime.file_name().map(|n| n.to_string_lossy()).unwrap_or_default())
}

/// The SID of the board's agents capability (see `agents_capability`), for an ACL (Windows).
#[cfg(windows)]
pub fn agent_capability_sid(runtime: &Path) -> Result<String, String> {
    let name = agents_capability(runtime);
    let sids = rappct::capability::derive_named_capability_sids(&[name.as_str()]).map_err(|e| format!("agents capability: {e}"))?;
    Ok(sids.first().ok_or("agents capability: no SID")?.sid_sddl.clone())
}

/// Runs an agent (see [`agent`]) to the end, with `env` on top of the person's environment, at
/// `cwd` (in its view); returns its exit code. On Windows it runs in an AppContainer.
pub fn run_agent(argv: &[OsString], p: &AgentPolicy, env: &[(OsString, OsString)], cwd: &Path) -> Result<i32, String> {
    #[cfg(windows)]
    {
        let writable = agent_writable(argv, p)?;
        appcontainer::agent(argv, p, &writable, env, cwd)
    }
    #[cfg(not(windows))]
    {
        let mut c = agent(argv, p)?;
        c.envs(env.iter().map(|(k, v)| (k, v))).current_dir(cwd);
        let status = c.status().map_err(|e| format!("could not start the agent: {e}"))?;
        Ok(status.code().unwrap_or(1))
    }
}

/// The checks every agent launch makes, and the directories it may write, resolved.
fn agent_writable(argv: &[OsString], p: &AgentPolicy) -> Result<Vec<PathBuf>, String> {
    let program = argv.first().ok_or("no agent command given")?;
    if program.to_string_lossy().starts_with('-') {
        return Err("the agent command cannot start with `-`".into());
    }
    let protected = vec![p.board.to_owned(), p.keys.to_owned()];
    let extra = refuse_secrets(&Policy { writable: p.extra.to_vec(), protected, ..Policy::default() }, Some(p.view))?.writable;
    Ok(std::iter::once(p.view.to_owned()).chain(p.ours.iter().cloned()).map(|p| canon(&p)).chain(extra).collect())
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned())
}

/// One argument for a Windows command line, quoted by the rules programs split it with (the
/// MSVC runtime's: backslashes are literal except before a quote).
#[cfg_attr(not(windows), allow(dead_code))]
fn quote_arg(arg: &OsStr) -> String {
    let s = arg.to_string_lossy();
    if !s.is_empty() && !s.contains([' ', '\t', '"']) {
        return s.into_owned();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for c in s.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// The program an agent command names (Windows): a path, or a name found on `PATH` as is or
/// with `.exe` or `.com`. A `.cmd` or `.bat` script would need cmd.exe and its own quoting
/// rules, so it is refused.
#[cfg(windows)]
fn resolve_exe(program: &OsStr) -> Result<PathBuf, String> {
    let given = Path::new(program);
    let found = if given.components().count() > 1 {
        given.is_file().then(|| given.to_owned())
    } else {
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&path).find_map(|d| {
            ["", ".exe", ".com"].iter().map(|ext| d.join(format!("{}{ext}", given.display()))).find(|c| c.is_file())
        })
    };
    let found = found.ok_or_else(|| format!("{}: not found", given.display()))?;
    match found.extension().map(|e| e.to_string_lossy().to_lowercase()).as_deref() {
        Some("cmd" | "bat") => Err(format!("{}: a .cmd or .bat script cannot run in the agent sandbox; run the program's .exe", found.display())),
        _ => Ok(found),
    }
}

/// An agent's command (`argv`), unable to act as the person (SPEC §13, local form). It writes
/// only its view, the temp directories, `ours` and `extra`; it never writes the board, never
/// reads a person key, and cannot reach what runs code outside the sandbox: other Unix sockets
/// (a tmux server, Docker, the user's D-Bus and systemd), LaunchServices and Apple events on
/// macOS, the person's terminal (`TIOCSTI`). Reads, the network and the keychain stay open:
/// an agent needs its tools and its model.
// (Windows: `run_agent` launches an AppContainer, see `appcontainer::agent`.)
#[cfg(not(windows))]
pub fn agent(argv: &[OsString], p: &AgentPolicy) -> Result<Command, String> {
    let writable = agent_writable(argv, p)?;
    let resolved = AgentPolicy { view: &canon(p.view), board: &canon(p.board), keys: &canon(p.keys), socket: &canon(p.socket), ..*p };
    let mut c = agent_backend(argv, &resolved, &writable)?;
    c.current_dir(p.view);
    Ok(c)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn agent_backend(_argv: &[OsString], _p: &AgentPolicy, _writable: &[PathBuf]) -> Result<Command, String> {
    Err("sandboxed agents are not available on this OS yet".into())
}

fn temp_dirs() -> Vec<PathBuf> {
    let mut v = vec![std::env::temp_dir(), PathBuf::from("/tmp")];
    v.dedup();
    v
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn backend(_cmd: &str, _dir: Option<&Path>, _policy: &Policy, _writable: &[PathBuf], _secrets: &[PathBuf]) -> Result<Command, String> {
    Err("no sandbox backend for this OS; set sandbox = false for this check".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refuse_secrets_t(p: &Policy) -> Result<Policy, String> {
        refuse_secrets(p, None)
    }

    #[test]
    fn windows_arguments_are_quoted_as_programs_split_them() {
        let q = |a: &str| quote_arg(OsStr::new(a));
        assert_eq!(q("plain"), "plain");
        assert_eq!(q(""), r#""""#);
        assert_eq!(q("two words"), r#""two words""#);
        assert_eq!(q(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(q(r"C:\dir with space\"), r#""C:\dir with space\\""#, "a trailing backslash is doubled before the quote");
        assert_eq!(q(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(q(r"C:\no\spaces"), r"C:\no\spaces", "backslashes are literal without quotes");
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
