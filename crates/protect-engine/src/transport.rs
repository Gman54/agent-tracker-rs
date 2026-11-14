//! Local transport: a per-user named pipe on Windows (remote clients rejected, first instance
//! required so another program can't squat the name), a Unix socket elsewhere (tests only).
use crate::engine::Engine;
use crate::errors::{Code, ProtectError};
use crate::framing::{self, FrameError};
use crate::rpc;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};

/// A connection that sends nothing for this long is closed.
pub const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

pub async fn handle_conn<S: AsyncRead + AsyncWrite + Unpin>(engine: Arc<Engine>, mut s: S) {
    let mut st = rpc::ConnState::default();
    loop {
        let body = match tokio::time::timeout(IDLE_TIMEOUT, framing::read_frame(&mut s)).await {
            Err(_) | Ok(Err(FrameError::Closed)) | Ok(Err(FrameError::Io(_))) => return,
            Ok(Err(FrameError::TooLarge(n))) => {
                let e = ProtectError::new(Code::FrameTooLarge, format!("A {n}-byte message is over the {}-byte limit; closing the connection.", framing::MAX_FRAME));
                let _ = framing::write_frame(&mut s, rpc::error_json(serde_json::Value::Null, -32600, &e).to_string().as_bytes()).await;
                return;
            }
            Ok(Ok(b)) => b,
        };
        let resp = rpc::handle_frame(&engine, &mut st, &body);
        if framing::write_frame(&mut s, resp.to_string().as_bytes()).await.is_err() {
            return;
        }
    }
}

#[cfg(windows)]
pub async fn serve(engine: Arc<Engine>, endpoint: &str) -> std::io::Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;
    let mut server = ServerOptions::new().first_pipe_instance(true).reject_remote_clients(true).create(endpoint)?;
    loop {
        server.connect().await?;
        let connected = server;
        server = ServerOptions::new().reject_remote_clients(true).create(endpoint)?;
        tokio::spawn(handle_conn(engine.clone(), connected));
    }
}

#[cfg(unix)]
pub async fn serve(engine: Arc<Engine>, endpoint: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::path::Path::new(endpoint);
    if path.exists() {
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            return Err(std::io::Error::new(std::io::ErrorKind::AddrInUse, "Another Protect engine is already running for this user."));
        }
        std::fs::remove_file(path)?;
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    loop {
        let (s, _) = listener.accept().await?;
        tokio::spawn(handle_conn(engine.clone(), s));
    }
}
