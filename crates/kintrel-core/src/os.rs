//! The few places Kintrel meets the OS differently on Unix and Windows.
//!
//! Windows has no executable bit and creating symlinks needs Developer Mode or a privilege, so
//! it does what Git for Windows does with `core.symlinks = false`: the store keeps modes and
//! links exactly, and a checkout that cannot make a link writes a plain file holding the target.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// A directory only this user may enter. (On Windows, per-user directories under the profile
/// are private already.)
pub fn set_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Creates a new directory only this user may enter.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::DirBuilderExt::mode(&mut fs::DirBuilder::new(), 0o700).create(dir)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(dir)
    }
}

pub fn is_executable(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

/// Sets a checked-out file's mode: 0755 when executable, else 0644. (Nothing to set on Windows.)
pub fn set_executable(path: &Path, exec: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 }))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, exec);
        Ok(())
    }
}

/// Writes a symlink to `target` (the stored link text). Where links cannot be made (Windows
/// without the privilege), writes a plain file holding the target instead, as Git does.
pub fn write_symlink(target: &[u8], path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), path)
    }
    #[cfg(windows)]
    {
        let text = String::from_utf8_lossy(target);
        match std::os::windows::fs::symlink_file(text.replace('/', "\\"), path) {
            Ok(()) => Ok(()),
            Err(_) => fs::write(path, target),
        }
    }
}

/// A symlink's target as stored: raw bytes on Unix, `/`-separated UTF-8 on Windows.
pub fn read_symlink(path: &Path) -> io::Result<Vec<u8>> {
    let target = fs::read_link(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(target.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        Ok(target.to_string_lossy().replace('\\', "/").into_bytes())
    }
}

/// The per-user cache directory: `$KINTREL_CACHE_DIR`, else `$XDG_CACHE_HOME`, else
/// `~/Library/Caches` (macOS), `~/.cache` (other Unix) or `%LOCALAPPDATA%` (Windows).
pub fn cache_dir() -> Option<PathBuf> {
    // Set by `kintrel agent` on Windows, where an AppContainer's LOCALAPPDATA is its own.
    if let Some(x) = std::env::var_os("KINTREL_CACHE_DIR") {
        return Some(PathBuf::from(x));
    }
    if let Some(x) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(x));
    }
    if cfg!(windows) {
        return std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    Some(if cfg!(target_os = "macos") { home.join("Library/Caches") } else { home.join(".cache") })
}

/// Something that changes when a file is replaced by another one at the same path (the
/// daemon's watchdog). Unix: device and inode. Windows: creation time and size, since the file
/// index is not in stable Rust.
pub fn file_identity(meta: &fs::Metadata) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    }
    #[cfg(not(unix))]
    {
        let created = meta.created().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as u64);
        (created, meta.len())
    }
}

/// The user's home directory: `HOME`, else `USERPROFILE` (Windows).
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}
