//! u32-BE length-prefixed JSON-RPC 2.0 frames over the daemon's endpoint (SPEC §12.1; see
//! `ipc` for Unix sockets and Windows named pipes).

use std::io;
use std::path::Path;

use kintrel_core::proto::{MAX_FRAME, Request, Response, RpcError};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Reads one frame. `Ok(None)` means the peer closed the connection between frames.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let len = match r.read_u32().await {
        Ok(n) => n,
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    };
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame of {len} bytes exceeds limit")));
    }
    let mut buf = vec![0; len as usize];
    r.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, body: &[u8]) -> io::Result<()> {
    let len = u32::try_from(body.len())
        .ok()
        .filter(|&n| n <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(body);
    w.write_all(&frame).await
}

#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Rpc(#[from] RpcError),
}

/// One connection to the daemon. Calls are sequential on a connection.
pub struct Client {
    stream: crate::ipc::ClientStream,
    next_id: u64,
}

impl Client {
    /// Retries briefly on `ConnectionRefused`: a burst of connects can overflow the listen
    /// backlog (128 on macOS) even while the daemon is accepting.
    pub async fn connect(socket: &Path) -> io::Result<Self> {
        let mut tries = 0;
        loop {
            match crate::ipc::connect(socket).await {
                Ok(stream) => return Ok(Client { stream, next_id: 1 }),
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused && tries < 40 => {
                    tries += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn call<P: Serialize, R: DeserializeOwned>(&mut self, method: &str, params: &P) -> Result<R, CallError> {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request { jsonrpc: "2.0".into(), id, method: method.into(), params: serde_json::to_value(params)? };
        write_frame(&mut self.stream, &serde_json::to_vec(&req)?).await?;
        let frame = read_frame(&mut self.stream)
            .await?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "daemon closed the connection"))?;
        let resp: Response = serde_json::from_slice(&frame)?;
        if resp.id != id {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "response id mismatch").into());
        }
        match (resp.error, resp.result) {
            (Some(e), _) => Err(e.into()),
            (None, r) => Ok(serde_json::from_value(r.unwrap_or_default())?),
        }
    }
}
