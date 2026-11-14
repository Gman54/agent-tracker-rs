//! Length-prefixed frames: 4-byte big-endian length, then that many bytes of UTF-8 JSON.
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Control frames are capped at 1 MiB.
pub const MAX_FRAME: usize = 1024 * 1024;

#[derive(Debug)]
pub enum FrameError {
    Closed,
    TooLarge(usize),
    Io(std::io::Error),
}

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>, FrameError> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(FrameError::Closed),
        Err(e) => return Err(FrameError::Io(e)),
    }
    let n = u32::from_be_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(FrameError::TooLarge(n));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).await.map_err(FrameError::Io)?;
    Ok(buf)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, body: &[u8]) -> std::io::Result<()> {
    if body.len() > MAX_FRAME {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame larger than 1 MiB"));
    }
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(body).await?;
    w.flush().await
}
