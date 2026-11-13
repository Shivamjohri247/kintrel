//! Windows: AppContainers. A check runs in a container of its own, inside a job object that
//! kills its whole process tree when the wait ends; an agent in one kept per view.

use super::{AgentPolicy, Policy, Running};
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

/// An AppContainer failure with its whole chain: the OS error is a launch error's source.
fn failed(what: &str, err: &dyn std::error::Error) -> String {
    let mut s = format!("AppContainer {what}: {err}");
    let mut cause = err.source();
    while let Some(c) = cause {
        s.push_str(&format!(": {c}"));
        cause = c.source();
    }
    s
}

/// An agent in an AppContainer of its own, one per view and kept across runs (so its grant
/// on the view stays valid), with the console. It writes `writable` (its view, its Claude
/// Code config and temp, `--writable` entries). Through the agents' capability it reads the
/// repository (the board in it), the runtime directory (the daemon pipe's name and key) and
/// the directories of its program and of Kintrel; the rest of the user profile, the person keys included, is
/// unreadable to it, as to every AppContainer. Network: the internet. Returns its exit code.
pub fn agent(argv: &[OsString], p: &AgentPolicy, writable: &[PathBuf], env: &[(OsString, OsString)], cwd: &Path) -> Result<i32, String> {
    let e = failed;
    let exe = super::resolve_exe(&argv[0])?;
    let tag = |q: Option<&Path>| q.and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // Views live at <runtime>/views/<session>, and the runtime directory is the board's id.
    let name = format!("kintrel.agent.{}.{}", tag(p.view.parent().and_then(Path::parent)), tag(Some(p.view)));
    let profile = AppContainerProfile::ensure(&name, "Kintrel agent", Some("A Kintrel agent's sandbox")).map_err(|x| e("profile", &x))?;
    for w in writable {
        grant(profile.sid.as_string(), w, "M", false)?;
    }
    let capability = super::agents_capability(p.socket);
    let agents = super::agent_capability_sid(p.socket)?;
    // Reads: the repository (the board in it, and the path the CLI opens), the daemon pipe's
    // name and key (not the rest of the runtime directory: other agents' views and tokens),
    // and the directories of the agent's program and of Kintrel. System directories are
    // readable to every AppContainer already, and only administrators may change them.
    let system: Vec<String> =
        ["SystemRoot", "ProgramFiles", "ProgramFiles(x86)"].iter().filter_map(|v| std::env::var(v).ok()).map(|v| v.to_lowercase()).collect();
    let readable_already = |d: &Path| {
        let d = d.to_string_lossy().to_lowercase();
        let d = d.trim_start_matches(r"\\?\");
        system.iter().any(|s| d.starts_with(s.as_str()))
    };
    let programs = [exe.parent().map(Path::to_owned), std::env::current_exe().ok().and_then(|k| k.parent().map(Path::to_owned))];
    let reads: Vec<PathBuf> = [p.board.parent().map(Path::to_owned), Some(p.socket.join("pipe")), Some(p.socket.join("key"))]
        .into_iter()
        .chain(programs)
        .flatten()
        .filter(|r| !readable_already(r))
        .collect();
    // None may be, hold or lie in a person key or a secret path: refused, not granted.
    let reads = super::refuse_secrets(&Policy { readable: reads, protected: vec![p.keys.to_owned()], ..Policy::default() }, None)?.readable;
    for r in &reads {
        grant(&agents, r, "RX", false)?;
    }
    // And explicitly: whatever else changes, this board's agents never read a person key.
    deny(&agents, p.keys)?;
    let sec = SecurityCapabilitiesBuilder::new(&profile.sid)
        .with_known(&[KnownCapability::InternetClient])
        .with_named(&[capability.as_str()])
        .build()
        .map_err(|x| e("capabilities", &x))?;
    // The person's environment (the agent's PATH, its API key), `env` on top, sorted by name.
    let mut vars: std::collections::BTreeMap<String, (OsString, OsString)> =
        std::env::vars_os().map(|(k, v)| (k.to_string_lossy().to_uppercase(), (k, v))).collect();
    for (k, v) in env {
        vars.insert(k.to_string_lossy().to_uppercase(), (k.clone(), v.clone()));
    }
    // Windows gives an AppContainer its own LOCALAPPDATA, so Kintrel inside would look for
    // its runtime directory there: name the person's (<cache>/kintrel/<board>) explicitly.
    if let Some(cache) = p.socket.parent().and_then(Path::parent) {
        vars.insert("KINTREL_CACHE_DIR".into(), ("KINTREL_CACHE_DIR".into(), cache.as_os_str().to_owned()));
    }
    let cmdline = argv.iter().skip(1).fold(super::quote_arg(exe.as_os_str()), |line, a| format!("{line} {}", super::quote_arg(a)));
    // At a terminal the agent shares the console. Otherwise (a script, `claude -p`, CI) it
    // gets pipes copied to and from ours: an inherited launch passes no handles, which left
    // it without stdio, unable even to start programs of its own.
    use std::io::IsTerminal;
    let console = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let opts = LaunchOptions {
        exe,
        cmdline: Some(cmdline),
        cwd: Some(cwd.to_owned()),
        env: Some(vars.into_values().collect()),
        stdio: if console { StdioConfig::Inherit } else { StdioConfig::Pipe },
        join_job: Some(JobLimits { kill_on_job_close: true, ..Default::default() }),
        ..Default::default()
    };
    let mut io = launch_in_container_with_io(&sec, &opts).map_err(|x| e("launch", &x))?;
    let copy = |from: Option<std::fs::File>, to: fn() -> Box<dyn std::io::Write + Send>| {
        from.map(|mut f| std::thread::spawn(move || std::io::copy(&mut f, &mut to()).ok()))
    };
    let out = copy(io.stdout.take(), || Box::new(std::io::stdout()));
    let err = copy(io.stderr.take(), || Box::new(std::io::stderr()));
    // Not joined: it ends with our stdin, or with the process.
    if let Some(mut stdin) = io.stdin.take() {
        std::thread::spawn(move || std::io::copy(&mut std::io::stdin(), &mut stdin).ok());
    }
    let code = io.wait(None).map_err(|x| e("wait", &x))?;
    for t in [out, err].into_iter().flatten() {
        t.join().ok();
    }
    Ok(code as i32)
}

pub fn start(cmd: &str, dir: Option<&Path>, policy: &Policy, env: &[(String, OsString)]) -> Result<Running, String> {
    let e = failed;
    // One container per run, granted only its checkout, so a check reaches no other
    // checkout; deleted when the check ends. Not one per checkout: checks of one snapshot
    // share a checkout and run at once, and would create, then delete, each other's (seen
    // on the first Windows CI run).
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = format!("kintrel.check.{}.{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
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
    // Sorted by name, ignoring case, as Windows expects an environment block to be.
    let mut env: Vec<(OsString, OsString)> = env.iter().map(|(k, v)| (OsString::from(k), v.clone())).collect();
    env.sort_by_key(|(k, _)| k.to_string_lossy().to_uppercase());
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

/// Adds an inheritable deny-everything ACE for `sid` to `dir` with `icacls`.
fn deny(sid: &str, dir: &Path) -> Result<(), String> {
    let status = Command::new("icacls")
        .arg(dir)
        .args(["/deny", &format!("*{sid}:(OI)(CI)F"), "/C", "/Q"])
        .status()
        .map_err(|e| format!("icacls: {e}"))?;
    if !status.success() {
        return Err(format!("icacls could not deny {} to {sid}", dir.display()));
    }
    Ok(())
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
    // One icacls at a time: each rewrites the whole ACL, so two checks granting their
    // containers the same checkout at once could drop each other's entry.
    static ACL: Mutex<()> = Mutex::new(());
    let _one = ACL.lock().unwrap();
    // A directory's entry is inherited and applied to what it holds; a file's is its own.
    let (inherit, walk): (&str, &[&str]) = if dir.is_dir() { ("(OI)(CI)", &["/T"]) } else { ("", &[]) };
    let status = Command::new("icacls")
        .arg(dir)
        .args(["/grant", &format!("*{sid}:{inherit}{rights}")])
        .args(walk)
        .args(["/C", "/Q"])
        .status()
        .map_err(|e| format!("icacls: {e}"))?;
    if !status.success() {
        return Err(format!("icacls could not grant {rights} on {} to {sid}", dir.display()));
    }
    done.lock().unwrap().insert(key);
    Ok(())
}
