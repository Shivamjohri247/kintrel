//! Path safety at the post boundary (SPEC §6 step 3).

/// A repo-relative path an agent may write: non-empty, `/`-separated, no absolute paths,
/// no `.`/`..`/empty segments, no backslashes or NULs, no `.git` segment anywhere and no
/// top-level `.kintrel`. Reserved names are matched the way the filesystem would see them
/// (see [`fs_alias`]), so `.GIT`, `.git.` or `.g\u{200c}it` are rejected too.
pub fn is_safe_rel_path(p: &str) -> bool {
    if p.is_empty() || p.starts_with('/') || p.contains(['\\', '\0']) {
        return false;
    }
    p.split('/').enumerate().all(|(i, seg)| {
        if seg.is_empty() || seg == "." || seg == ".." {
            return false;
        }
        let name = fs_alias(seg);
        name != ".git" && name != "git~1" && !(i == 0 && (name == ".kintrel" || name == "kintre~1"))
    })
}

/// What a case-insensitive filesystem may resolve `seg` to: lowercased, with the code points
/// HFS+ ignores removed and trailing dots/spaces trimmed (NTFS). Mirrors Git's
/// `is_hfs_dotgit` / `is_ntfs_dotgit` checks.
fn fs_alias(seg: &str) -> String {
    let ignorable = |c: char| {
        matches!(c, '\u{200c}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{206a}'..='\u{206f}' | '\u{feff}')
    };
    let s: String = seg.chars().filter(|&c| !ignorable(c)).flat_map(char::to_lowercase).collect();
    s.trim_end_matches(['.', ' ']).to_owned()
}

#[cfg(test)]
mod tests {
    use super::is_safe_rel_path as ok;

    #[test]
    fn rejects_escapes_and_reserved_dirs() {
        for good in ["src/cart.rs", ".github/workflows/ci.yml", "a/.gitignore", "a/.kintrel/x", ".gitmodules"] {
            assert!(ok(good), "{good:?} should be allowed");
        }
        for bad in ["", "/etc/passwd", "../x", "a/../b", "a//b", "a/./b", "a/", ".git/config",
                    ".kintrel/log.db", ".git", "a\\b", "a\0b", ".GIT/config", ".Kintrel/x",
                    "sub/.git/hooks/post-checkout", ".git./config", ".git /config",
                    ".g\u{200c}it/config", "\u{feff}.git/config", "git~1/config", "GIT~1/x"] {
            assert!(!ok(bad), "{bad:?} should be rejected");
        }
    }
}
