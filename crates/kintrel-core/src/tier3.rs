//! Tier 3 (SPEC §7.3): Claude proposes a merge where Tiers 1–2 conflicted. Proposals are never
//! trusted: [`crate::merge::proposal_keeps_sides`] and the parse check filter them here, and
//! the gate's checks judge them like any other note.
//!
//! Two backends: the Messages API through `curl` when `ANTHROPIC_API_KEY` is set (no TLS stack
//! in this workspace), else the logged-in Claude Code CLI (`claude -p`, no tools, no settings or hooks).
//! Either way the model answers with the merged file between fixed markers, or `UNRESOLVABLE`.

use std::io::Write;
use std::process::{Command, Stdio};

pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
const OPEN: &str = "<<<KINTREL-MERGED";
const CLOSE: &str = "KINTREL-MERGED>>>";

/// The merged file, or why there is none (unresolvable, refused, a backend failure).
pub fn propose(path: &str, base: &[u8], ours: &[u8], theirs: &[u8], model: &str) -> Result<Vec<u8>, String> {
    propose_with(path, base, ours, theirs, model, None)
}

/// [`propose`], or with `command` (a program and its arguments) as the backend: it gets the
/// prompt on stdin and prints the reply.
pub fn propose_with(path: &str, base: &[u8], ours: &[u8], theirs: &[u8], model: &str, command: Option<&[String]>) -> Result<Vec<u8>, String> {
    let prompt = prompt(path, base, ours, theirs);
    let reply = match (command, std::env::var("ANTHROPIC_API_KEY")) {
        (Some([program, args @ ..]), _) => run(Command::new(program).args(args), prompt.as_bytes())?,
        (Some([]), _) => return Err("tier3_command is empty".into()),
        (None, Ok(key)) if !key.is_empty() => api(&prompt, model, &key)?,
        (None, _) => cli(&prompt, model)?,
    };
    extract(&reply)
}

fn prompt(path: &str, base: &[u8], ours: &[u8], theirs: &[u8]) -> String {
    let show = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    format!(
        "Two developers edited the file `{path}` concurrently, starting from the same base. A \
         three-way merge conflicted. Produce the merged file that keeps both developers' intent.\n\n\
         Rules:\n\
         - Keep every change either side made, unless the two changes truly contradict each other.\n\
         - Where both sides changed the same code, combine them if their intent is compatible.\n\
         - Change nothing else: no reformatting, no new comments, no fixes the developers did not make.\n\
         - If the two intents cannot both be kept, answer only with the word UNRESOLVABLE.\n\n\
         Answer with the complete merged file between a line `{OPEN}` and a line `{CLOSE}`, and \
         nothing else.\n\n\
         <base>\n{}\n</base>\n\n<ours>\n{}\n</ours>\n\n<theirs>\n{}\n</theirs>\n",
        show(base),
        show(ours),
        show(theirs)
    )
}

/// The file between the markers, exactly; `UNRESOLVABLE` or anything malformed is an error.
fn extract(reply: &str) -> Result<Vec<u8>, String> {
    let Some(start) = reply.find(OPEN) else {
        return Err(if reply.contains("UNRESOLVABLE") { "unresolvable".into() } else { "no merged file in the reply".into() });
    };
    let body = &reply[start + OPEN.len()..];
    let body = body.strip_prefix("\r\n").or_else(|| body.strip_prefix('\n')).unwrap_or(body);
    let end = body.find(CLOSE).ok_or("unterminated merged file")?;
    Ok(body.as_bytes()[..end].to_vec())
}

/// The Messages API, raw HTTP through curl. The key goes in a private config file, never on
/// the command line (where other users could read it in the process list).
fn api(prompt: &str, model: &str, key: &str) -> Result<String, String> {
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 32000,
        "output_config": { "effort": "high" },
        // Re-run a safety decline on Anthropic's recommended fallback model, server-side.
        "fallbacks": "default",
        "messages": [{ "role": "user", "content": prompt }],
    });
    let dir = std::env::temp_dir().join(format!("kintrel-tier3-{}-{}", std::process::id(), nanos()));
    crate::os::create_private_dir(&dir).map_err(|e| format!("temp dir: {e}"))?;
    let cfg = dir.join("curl.cfg");
    let escaped = key.replace('\\', "\\\\").replace('"', "\\\"");
    let written = std::fs::write(&cfg, format!("header = \"x-api-key: {escaped}\"\n"));
    let out = written.map_err(|e| format!("curl config: {e}")).and_then(|()| {
        run(
            Command::new("curl")
                .args(["-sS", "--max-time", "900", "-K"])
                .arg(&cfg)
                .args(["-H", "content-type: application/json", "-H", "anthropic-version: 2026-10-21"])
                .args(["-H", "anthropic-beta: server-side-fallback-2026-07-01"])
                .args(["--data-binary", "@-", "https://api.anthropic.com/v1/messages"]),
            body.to_string().as_bytes(),
        )
    });
    std::fs::remove_dir_all(&dir).ok();
    let response: serde_json::Value = serde_json::from_str(&out?).map_err(|e| format!("API response: {e}"))?;
    if let Some(err) = response.get("error") {
        return Err(format!("API error: {err}"));
    }
    if response["stop_reason"] == "refusal" {
        return Err("the model declined".into());
    }
    let text: String = response["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    Ok(text)
}

/// The Claude Code CLI: no tools, no settings or hooks, no saved session, in an empty
/// directory, the prompt on stdin.
fn cli(prompt: &str, model: &str) -> Result<String, String> {
    let dir = std::env::temp_dir().join(format!("kintrel-tier3-cli-{}-{}", std::process::id(), nanos()));
    crate::os::create_private_dir(&dir).map_err(|e| format!("temp dir: {e}"))?;
    let out = run(
        Command::new("claude")
            // `--bare` would also skip the login, so settings (and their hooks) are dropped with
            // `--setting-sources ""` instead.
            .args(["-p", "--setting-sources", "", "--tools", "", "--no-session-persistence", "--output-format", "json", "--model", model])
            .current_dir(&dir),
        prompt.as_bytes(),
    );
    std::fs::remove_dir_all(&dir).ok();
    let response: serde_json::Value = serde_json::from_str(&out?).map_err(|e| format!("claude -p output: {e}"))?;
    if response["is_error"] == true {
        return Err(format!("claude -p: {}", response["result"]));
    }
    response["result"].as_str().map(str::to_owned).ok_or_else(|| "claude -p: no result".into())
}

fn run(cmd: &mut Command, input: &[u8]) -> Result<String, String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{:?}: {e}", cmd.get_program()))?;
    let mut stdin = child.stdin.take().expect("piped");
    let input = input.to_vec();
    // Write on a thread: a large prompt would otherwise deadlock against a full stdout pipe.
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    writer.join().ok();
    if !out.status.success() {
        return Err(format!("{:?} failed: {}", cmd.get_program(), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
}

/// Whether a backend is available: an API key, or the `claude` CLI on `PATH`.
pub fn available() -> bool {
    std::env::var("ANTHROPIC_API_KEY").is_ok_and(|k| !k.is_empty())
        || std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("claude").is_file() || d.join("claude.exe").is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_are_parsed_strictly() {
        assert_eq!(extract("sure:\n<<<KINTREL-MERGED\nfn a() {}\nKINTREL-MERGED>>>\n").unwrap(), b"fn a() {}\n");
        assert_eq!(extract("UNRESOLVABLE").unwrap_err(), "unresolvable");
        assert!(extract("<<<KINTREL-MERGED\nfn a() {}\n").is_err(), "unterminated");
        assert!(extract("here is the file: fn a() {}").is_err(), "no markers");
    }
}
