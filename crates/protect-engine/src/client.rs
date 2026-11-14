//! Minimal Rust client: framing, typed errors and challenge sign-in. A transport failure is an
//! error, never an implicit "allow".
use crate::errors::Code;
use crate::framing;
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};

pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

#[derive(Debug, Clone)]
pub struct ClientError {
    /// None means the transport failed; the outcome of the request is unknown.
    pub code: Option<Code>,
    pub message: String,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

pub struct Client {
    s: Box<dyn Stream>,
    next: u64,
}

pub fn generate_key() -> SigningKey {
    SigningKey::generate(&mut rand::rngs::OsRng)
}

pub fn public_key_b64(k: &SigningKey) -> String {
    base64::engine::general_purpose::STANDARD.encode(k.verifying_key().to_bytes())
}

/// Loads the app's pairing key from `path`, or makes a new one and saves it there. The file
/// holds the 32-byte secret as base64 and lives in the app's own data folder, so the app keeps
/// its pairing across restarts. A file that exists but can't be read is an error: silently
/// making a new key would quietly break the pairing.
pub fn load_or_create_key(path: &std::path::Path) -> std::io::Result<SigningKey> {
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: {m}", path.display()));
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let bytes: [u8; 32] = base64::engine::general_purpose::STANDARD.decode(text.trim()).map_err(|_| bad("not base64"))?.try_into().map_err(|_| bad("not a 32-byte key"))?;
            Ok(SigningKey::from_bytes(&bytes))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let k = generate_key();
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(path, base64::engine::general_purpose::STANDARD.encode(k.to_bytes()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
            Ok(k)
        }
        Err(e) => Err(e),
    }
}

impl Client {
    pub async fn connect(endpoint: &str) -> std::io::Result<Self> {
        Ok(Self { s: open(endpoint).await?, next: 1 })
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        let id = format!("req-{}", self.next);
        self.next += 1;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let io = |e: std::io::Error| ClientError { code: None, message: format!("connection problem: {e}") };
        framing::write_frame(&mut self.s, req.to_string().as_bytes()).await.map_err(io)?;
        let body = framing::read_frame(&mut self.s).await.map_err(|e| ClientError { code: None, message: format!("connection problem: {e:?}") })?;
        let resp: Value = serde_json::from_slice(&body).map_err(|e| ClientError { code: None, message: format!("bad reply: {e}") })?;
        if let Some(err) = resp.get("error") {
            let code = err.pointer("/data/protect_code").cloned().and_then(|c| serde_json::from_value(c).ok());
            return Err(ClientError { code, message: err.get("message").and_then(Value::as_str).unwrap_or("").to_string() });
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Challenge, sign, prove. Returns the granted scopes.
    pub async fn sign_in(&mut self, app_id: &str, key: &SigningKey) -> Result<Value, ClientError> {
        let ch = self.call("v1.auth.challenge", json!({"protocol_major": crate::PROTOCOL_MAJOR, "app_id": app_id})).await?;
        let msg = crate::engine::auth_message(ch["engine_id"].as_str().unwrap_or(""), ch["nonce"].as_str().unwrap_or(""));
        let sig = base64::engine::general_purpose::STANDARD.encode(key.sign(&msg).to_bytes());
        self.call("v1.auth.prove", json!({"protocol_major": crate::PROTOCOL_MAJOR, "app_id": app_id, "signature": sig})).await
    }
}

#[cfg(windows)]
async fn open(endpoint: &str) -> std::io::Result<Box<dyn Stream>> {
    use tokio::net::windows::named_pipe::ClientOptions;
    const ERROR_PIPE_BUSY: i32 = 231;
    for _ in 0..50 {
        match ClientOptions::new().open(endpoint) {
            Ok(c) => return Ok(Box::new(c)),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "the Protect engine stayed busy"))
}

#[cfg(unix)]
async fn open(endpoint: &str) -> std::io::Result<Box<dyn Stream>> {
    Ok(Box::new(tokio::net::UnixStream::connect(endpoint).await?))
}
