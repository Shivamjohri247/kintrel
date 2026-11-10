//! The daemon's local endpoint: a Unix domain socket, or a named pipe on Windows.
//!
//! Unix: `<runtime>/kintrel.sock`, mode 0600 in a 0700 directory. Windows: a named pipe with a
//! random name per daemon start (`\\.\pipe\kintrel-<128 random bits>`), written to
//! `<runtime>/pipe` in the per-user profile. Every user can list pipe names, so the name alone
//! proves nothing: another user could reuse a stale one and collect clients' session tokens.
//! The daemon also writes a random key to `<runtime>/key`, which other users cannot read, and
//! answers each new client's random challenge with a keyed BLAKE3 hash; a client sends nothing
//! until the answer checks out. The daemon creates the first pipe instance exclusively and
//! refuses remote clients; its ACL also admits agents sandboxed by `kintrel agent` (see
//! `Listener::listener`). On Windows the endpoint passed around is the runtime directory.

use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
pub type ClientStream = tokio::net::UnixStream;
#[cfg(windows)]
pub type ClientStream = interprocess::os::windows::named_pipe::tokio::DuplexPipeStream<interprocess::os::windows::named_pipe::pipe_mode::Bytes>;

#[cfg(unix)]
pub type ServerStream = tokio::net::UnixStream;
#[cfg(windows)]
pub type ServerStream = interprocess::os::windows::named_pipe::tokio::DuplexPipeStream<interprocess::os::windows::named_pipe::pipe_mode::Bytes>;

/// The endpoint for the board whose runtime directory is `runtime`: the socket on Unix, the
/// runtime directory itself on Windows (it holds the pipe's name and key).
pub fn endpoint(runtime: &Path) -> PathBuf {
    if cfg!(windows) { runtime.to_owned() } else { runtime.join("kintrel.sock") }
}

/// The keyed hash a daemon answers a challenge with.
#[cfg_attr(not(windows), allow(dead_code))]
fn answer_to(key: &str, challenge: &[u8; 32]) -> blake3::Hash {
    blake3::keyed_hash(blake3::hash(key.as_bytes()).as_bytes(), challenge)
}

/// 128 bits from the OS's randomness, through std's randomly keyed hasher (no new dependency).
pub fn random_hex() -> String {
    use std::hash::{BuildHasher, Hasher};
    let half = || {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(0);
        h.finish()
    };
    format!("{:016x}{:016x}", half(), half())
}

pub async fn connect(endpoint: &Path) -> io::Result<ClientStream> {
    #[cfg(unix)]
    {
        tokio::net::UnixStream::connect(endpoint).await
    }
    #[cfg(windows)]
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        const ERROR_PIPE_BUSY: i32 = 231;
        let refused = |why: &str| io::Error::new(io::ErrorKind::ConnectionRefused, why.to_owned());
        let read = |file: &str, absent: &str| match std::fs::read_to_string(endpoint.join(file)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(refused(absent)),
            Err(e) => Err(io::Error::new(e.kind(), format!("reading {}: {e}", endpoint.join(file).display()))),
            Ok(v) => Ok(v),
        };
        let name = read("pipe", "no daemon has started")?;
        let key = read("key", "no daemon key")?;
        let name = name.trim();
        if !name.starts_with(r"\\.\pipe\kintrel-") {
            return Err(refused("bad pipe name"));
        }
        let mut tries = 0;
        let mut pipe = loop {
            match open_pipe(name) {
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && tries < 40 => {
                    tries += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                // No pipe yet reads as "nobody listening", like a refused socket.
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(io::Error::new(io::ErrorKind::ConnectionRefused, e)),
                Err(e) => return Err(io::Error::new(e.kind(), format!("opening the daemon's pipe: {e}"))),
                Ok(p) => break p,
            }
        };
        // Prove the other end is this user's daemon before sending anything (a session token).
        let challenge: [u8; 32] = *blake3::hash(random_hex().as_bytes()).as_bytes();
        pipe.write_all(&challenge).await?;
        let mut answer = [0u8; 32];
        pipe.read_exact(&mut answer).await?;
        if blake3::Hash::from(answer) != answer_to(key.trim(), &challenge) {
            return Err(refused("the pipe did not prove it is this board's daemon"));
        }
        Ok(pipe)
    }
}

/// The access a client opens the pipe with, and all the pipe grants agents: read, write data and
/// write attributes. Not `GENERIC_WRITE`, whose FILE_APPEND_DATA is, on a pipe, the right to
/// create a server instance of it, with which a client could intercept others.
#[cfg(windows)]
const CLIENT_ACCESS: u32 = 0x0012_018B; // FILE_GENERIC_READ | FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES

/// Opens the pipe with exactly [`CLIENT_ACCESS`], for overlapped I/O, and at the identification
/// level only (the server cannot act as the client), as a tokio stream.
#[cfg(windows)]
fn open_pipe(name: &str) -> io::Result<ClientStream> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
    const SECURITY_SQOS_PRESENT: u32 = 0x0010_0000;
    const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;
    let file = std::fs::OpenOptions::new()
        .access_mode(CLIENT_ACCESS)
        .custom_flags(FILE_FLAG_OVERLAPPED)
        .security_qos_flags(SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION)
        .open(name)?;
    ClientStream::try_from(std::os::windows::io::OwnedHandle::from(file)).map_err(|e| io::Error::other(e.to_string()))
}

/// Answers a new client's challenge (Windows; nothing to do on Unix, where the socket's
/// permissions already keep other users out).
pub async fn answer(stream: &mut ServerStream, listener_key: &str) -> io::Result<()> {
    #[cfg(windows)]
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut challenge = [0u8; 32];
        tokio::time::timeout(std::time::Duration::from_secs(5), stream.read_exact(&mut challenge))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no challenge"))??;
        stream.write_all(answer_to(listener_key, &challenge).as_bytes()).await
    }
    #[cfg(unix)]
    {
        let _ = (stream, listener_key);
        Ok(())
    }
}

pub struct Listener {
    /// The handshake key (Windows); empty on Unix.
    pub key: String,
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
    #[cfg(windows)]
    inner: interprocess::os::windows::named_pipe::tokio::PipeListener<
        interprocess::os::windows::named_pipe::pipe_mode::Bytes,
        interprocess::os::windows::named_pipe::pipe_mode::Bytes,
    >,
}

impl Listener {
    /// Binds the board's endpoint in `runtime`. `AddrInUse` if a daemon already serves it.
    pub async fn bind(runtime: &Path) -> io::Result<Listener> {
        #[cfg(unix)]
        {
            let endpoint = &endpoint(runtime);
            use std::os::unix::fs::PermissionsExt;
            if tokio::net::UnixStream::connect(endpoint).await.is_ok() {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, "a daemon is already running for this board"));
            }
            match std::fs::remove_file(endpoint) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
            let inner = tokio::net::UnixListener::bind(endpoint)?;
            std::fs::set_permissions(endpoint, std::fs::Permissions::from_mode(0o600))?;
            Ok(Listener { key: String::new(), inner })
        }
        #[cfg(windows)]
        {
            // A pipe that fails the handshake (a stale name another user took) is no daemon.
            if connect(&endpoint(runtime)).await.is_ok() {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, "a daemon is already running for this board"));
            }
            let name = PathBuf::from(format!(r"\\.\pipe\kintrel-{}", random_hex()));
            let inner = Self::listener(&name, runtime)?;
            let key = format!("{}{}", random_hex(), random_hex());
            // Publish the key, then the name, once the pipe exists; write-then-rename so readers
            // never see half of either.
            for (file, value) in [("key", key.as_str()), ("pipe", &*name.to_string_lossy())] {
                let tmp = runtime.join(format!("{file}.tmp"));
                std::fs::write(&tmp, value.as_bytes())?;
                std::fs::rename(&tmp, runtime.join(file))?;
            }
            Ok(Listener { key, inner })
        }
    }

    /// The pipe: created exclusively (the first instance), local clients only, and open to its
    /// owner, SYSTEM and administrators, plus this board's agents run through `kintrel agent`,
    /// whose AppContainer carries the board's agents capability, with [`CLIENT_ACCESS`] only. (No Low integrity label: that would also
    /// admit any Low-integrity process of the same user, such as a browser's sandbox.)
    #[cfg(windows)]
    fn listener(
        name: &Path,
        runtime: &Path,
    ) -> io::Result<
        interprocess::os::windows::named_pipe::tokio::PipeListener<
            interprocess::os::windows::named_pipe::pipe_mode::Bytes,
            interprocess::os::windows::named_pipe::pipe_mode::Bytes,
        >,
    > {
        use interprocess::os::windows::named_pipe::{PipeListenerOptions, pipe_mode};
        let agents = kintrel_ci::sandbox::agent_capability_sid(runtime).map_err(io::Error::other)?;
        let sddl = format!("D:P(A;;GA;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x{CLIENT_ACCESS:X};;;{agents})");
        let sddl = widestring::U16CString::from_str(&sddl).map_err(io::Error::other)?;
        let sd = interprocess::os::windows::security_descriptor::SecurityDescriptor::deserialize(&sddl)?;
        PipeListenerOptions::new().path(name.as_os_str()).security_descriptor(Some(sd)).create_tokio_duplex::<pipe_mode::Bytes>()
    }

    pub async fn accept(&mut self) -> io::Result<ServerStream> {
        #[cfg(unix)]
        {
            Ok(self.inner.accept().await?.0)
        }
        #[cfg(windows)]
        {
            self.inner.accept().await
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn pipe_names_are_unguessable() {
        let (a, b) = (super::random_hex(), super::random_hex());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn only_the_key_holder_answers_a_challenge() {
        let c = [7u8; 32];
        assert_eq!(super::answer_to("key", &c), super::answer_to("key", &c));
        assert_ne!(super::answer_to("key", &c), super::answer_to("other", &c));
        assert_ne!(super::answer_to("key", &c), super::answer_to("key", &[8u8; 32]));
    }
}
