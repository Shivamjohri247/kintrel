//! Tier 2: structured merge with Mergiraf, run as a subprocess (SPEC §7.3; GPL-3.0, so never
//! linked). Used only where Tier 1 left a conflict. Every combiner pass refolds, so results are
//! cached by input hashes.
//!
//! The binary is `$KINTREL_MERGIRAF` if set (`off` disables Tier 2), else `mergiraf` on `PATH`.
// ponytail: whole-file merge per conflicting file; per-atom invocation if B6 shows it matters.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use crate::Hash;

fn binary() -> Option<&'static PathBuf> {
    static BIN: OnceLock<Option<PathBuf>> = OnceLock::new();
    BIN.get_or_init(|| match std::env::var_os("KINTREL_MERGIRAF") {
        Some(v) if v == "off" => None,
        Some(v) => Some(PathBuf::from(v)),
        None => std::env::var_os("PATH")?.to_str()?.split(':').map(|d| PathBuf::from(d).join("mergiraf")).find(|p| p.is_file()),
    })
    .as_ref()
}

/// A clean merge of the whole file, or `None` (no Mergiraf, a conflict, or a failure).
/// The caller still checks that the result parses (Tier 4).
pub fn merge(path: &str, base: &[u8], ours: &[u8], theirs: &[u8]) -> Option<Vec<u8>> {
    let bin = binary()?;
    static CACHE: OnceLock<Mutex<HashMap<Hash, Option<Vec<u8>>>>> = OnceLock::new();
    let key = {
        let mut h = blake3::Hasher::new();
        for part in [path.as_bytes(), base, ours, theirs] {
            h.update(&(part.len() as u64).to_le_bytes());
            h.update(part);
        }
        Hash(*h.finalize().as_bytes())
    };
    let cache = CACHE.get_or_init(Default::default);
    if let Some(hit) = cache.lock().unwrap().get(&key) {
        return hit.clone();
    }
    let result = run(bin, path, base, ours, theirs);
    cache.lock().unwrap().insert(key, result.clone());
    result
}

pub(crate) fn run(bin: &PathBuf, path: &str, base: &[u8], ours: &[u8], theirs: &[u8]) -> Option<Vec<u8>> {
    // Mergiraf picks the language from the extension, so the temp files keep it.
    let ext = path.rsplit('/').next()?.rsplit_once('.').map_or(String::new(), |(_, e)| format!(".{e}"));
    // A fresh directory: create_dir fails if anything (say, a planted symlink) is already there.
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!("kintrel-tier2-{}-{n}-{nanos}", std::process::id()));
    // 0700: the files are source code, and /tmp is shared on Linux.
    crate::os::create_private_dir(&dir).ok()?;
    let (b, o, t, out) = (dir.join(format!("base{ext}")), dir.join(format!("ours{ext}")), dir.join(format!("theirs{ext}")), dir.join(format!("out{ext}")));
    let written = std::fs::write(&b, base).and(std::fs::write(&o, ours)).and(std::fs::write(&t, theirs));
    let ok = written.is_ok()
        && Command::new(bin).arg("merge").arg(&b).arg(&o).arg(&t).arg("-o").arg(&out).output().is_ok_and(|r| r.status.success());
    let merged = ok.then(|| std::fs::read(&out).ok()).flatten();
    std::fs::remove_dir_all(&dir).ok();
    merged.filter(|m| !m.windows(7).any(|w| w == b"<<<<<<<"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    /// A stand-in for Mergiraf: checks it got `merge BASE OURS THEIRS -o OUT` with the file's
    /// extension, then either writes a merge or exits 1 (conflict) if theirs asks it to.
    #[cfg(unix)] // the stand-in is a shell script
    #[test]
    fn runs_the_binary_and_accepts_only_clean_merges() {
        let dir = std::env::temp_dir().join(format!("kintrel-fake-mergiraf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("mergiraf");
        std::fs::write(
            &bin,
            "#!/bin/sh\n[ \"$1\" = merge ] && [ \"$5\" = -o ] || exit 2\ncase \"$2\" in *.py) ;; *) exit 3;; esac\n\
             grep -q CONFLICT \"$4\" && exit 1\ngrep -q MARKERS \"$4\" && { printf '<<<<<<< ours\\n' > \"$6\"; exit 0; }\n\
             printf 'merged\\n' > \"$6\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(run(&bin, "src/a.py", b"b", b"o", b"t"), Some(b"merged\n".to_vec()));
        assert_eq!(run(&bin, "src/a.py", b"b", b"o", b"CONFLICT"), None, "exit 1 is a conflict");
        assert_eq!(run(&bin, "src/a.py", b"b", b"o", b"MARKERS"), None, "markers are never accepted");
        assert_eq!(run(&bin, "src/a.rs", b"b", b"o", b"t"), None, "the extension reaches the tool");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The real Mergiraf, if installed: both sides change the same line (a line merge conflicts)
    /// but different arguments of one call (a structured merge does not).
    #[test]
    fn real_mergiraf_merges_one_line_edits() {
        let Some(bin) = binary() else { return eprintln!("mergiraf not installed; skipped") };
        let base = b"def f(a, b):\n    return g(a, b)\n";
        let ours = b"def f(a, b):\n    return g(a, b, c=1)\n";
        let theirs = b"def f(a, b):\n    return g(z, a, b)\n";
        assert_eq!(run(bin, "m.py", base, ours, theirs).as_deref(), Some(&b"def f(a, b):\n    return g(z, a, b, c=1)\n"[..]));
    }
}
