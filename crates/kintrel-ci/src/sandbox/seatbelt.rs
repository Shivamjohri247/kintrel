//! macOS: `sandbox-exec` with a Seatbelt profile built per run.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{AgentPolicy, Policy, temp_dirs};

/// A Seatbelt string literal: backslashes and quotes escaped.
fn lit(p: &Path) -> String {
    format!("\"{}\"", p.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\""))
}

fn sandbox_exec(profile: String) -> Command {
    let mut c = Command::new("/usr/bin/sandbox-exec");
    c.arg("-p").arg(profile);
    c
}

pub(super) fn backend(cmd: &str, dir: Option<&Path>, policy: &Policy, writable: &[PathBuf], secrets: &[PathBuf]) -> Result<Command, String> {
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
    let mut c = sandbox_exec(profile);
    c.args(["/bin/sh", "-c", cmd]);
    Ok(c)
}

pub(super) fn agent_backend(argv: &[OsString], p: &AgentPolicy, writable: &[PathBuf]) -> Result<Command, String> {
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
    let mut c = sandbox_exec(profile);
    c.args(argv);
    Ok(c)
}
