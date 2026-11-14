//! Optional replay-capture store: exact (unredacted) request payloads, encrypted with
//! ChaCha20-Poly1305, retention-limited and deletable. Off unless explicitly enabled.
//!
//! Key protection:
//! - Windows: the 32-byte key is wrapped with DPAPI (`CryptProtectData`, current-user scope).
//!   NOTE: this code path is compiled only on Windows and has not been built or run in the Linux
//!   development environment used for this change.
//! - Other platforms: the raw key is stored in a file with mode 0600. This is weaker: anyone who can
//!   read the user's files can read the key.
//! Replay permission is separate from ordinary event access: the UI must explicitly request a
//! replay, and the vault is never read by the normal record views.
use crate::{Result, RtError};
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::RngCore;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct ReplayVault {
    cipher: ChaCha20Poly1305,
    pub key_protection: &'static str,
}

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

impl ReplayVault {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("replay.key");
        let key = if path.exists() { unwrap_key(&std::fs::read(&path)?)? } else {
            let mut k = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut k);
            write_private(&path, &wrap_key(&k)?)?;
            k
        };
        Ok(Self { cipher: ChaCha20Poly1305::new(Key::from_slice(&key)), key_protection: KEY_PROTECTION })
    }

    pub fn store(&self, conn: &Connection, model_request_id: &str, payload: &Value, retention_days: u32) -> Result<()> {
        let mut nonce = [0u8; 12];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let ct = self.cipher.encrypt(Nonce::from_slice(&nonce), serde_json::to_vec(payload)?.as_slice()).map_err(|_| RtError::Invalid("encryption failed".into()))?;
        let now = crate::now_ms();
        conn.execute(
            "INSERT OR REPLACE INTO rt_replay_store(model_request_id, nonce, ciphertext, created_at, expires_at) VALUES (?1,?2,?3,?4,?5)",
            params![model_request_id, B64.encode(nonce), B64.encode(ct), now, now + retention_days.max(1) as i64 * 86_400_000],
        )?;
        Ok(())
    }

    pub fn load(&self, conn: &Connection, model_request_id: &str) -> Result<Option<Value>> {
        let row: Option<(String, String, i64)> = conn
            .query_row("SELECT nonce, ciphertext, expires_at FROM rt_replay_store WHERE model_request_id=?1", [model_request_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        let Some((n, c, exp)) = row else { return Ok(None) };
        if exp < crate::now_ms() {
            return Ok(None);
        }
        let nonce = B64.decode(n).map_err(|e| RtError::Invalid(e.to_string()))?;
        let ct = B64.decode(c).map_err(|e| RtError::Invalid(e.to_string()))?;
        let pt = self.cipher.decrypt(Nonce::from_slice(&nonce), ct.as_slice()).map_err(|_| RtError::Invalid("replay record failed authentication".into()))?;
        Ok(Some(serde_json::from_slice(&pt)?))
    }
}

pub fn purge_expired(conn: &Connection) -> Result<usize> {
    Ok(conn.execute("DELETE FROM rt_replay_store WHERE expires_at < ?1", [crate::now_ms()])?)
}

pub fn delete_all(conn: &Connection) -> Result<usize> {
    Ok(conn.execute("DELETE FROM rt_replay_store", [])?)
}

pub fn delete_one(conn: &Connection, model_request_id: &str) -> Result<usize> {
    Ok(conn.execute("DELETE FROM rt_replay_store WHERE model_request_id=?1", [model_request_id])?)
}

pub fn vault_dir(home: &Path) -> PathBuf {
    home.join("runtime")
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
        f.write_all(data)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, data)?;
        Ok(())
    }
}

#[cfg(not(windows))]
const KEY_PROTECTION: &str = "file (mode 0600, not OS-encrypted)";
#[cfg(windows)]
const KEY_PROTECTION: &str = "Windows DPAPI (current user)";

#[cfg(not(windows))]
fn wrap_key(k: &[u8; 32]) -> Result<Vec<u8>> {
    Ok(k.to_vec())
}

#[cfg(not(windows))]
fn unwrap_key(b: &[u8]) -> Result<[u8; 32]> {
    b.try_into().map_err(|_| RtError::Invalid("replay key file is corrupt".into()))
}

#[cfg(windows)]
fn wrap_key(k: &[u8; 32]) -> Result<Vec<u8>> {
    dpapi::protect(k)
}

#[cfg(windows)]
fn unwrap_key(b: &[u8]) -> Result<[u8; 32]> {
    dpapi::unprotect(b)?.as_slice().try_into().map_err(|_| RtError::Invalid("replay key has wrong length".into()))
}

#[cfg(windows)]
mod dpapi {
    use crate::{Result, RtError};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB};

    fn call(input: &[u8], protect: bool) -> Result<Vec<u8>> {
        let mut inb = CRYPT_INTEGER_BLOB { cbData: input.len() as u32, pbData: input.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: pointers are valid for the duration of the call; the output buffer is freed with LocalFree.
        let ok = unsafe {
            if protect {
                CryptProtectData(&mut inb, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), 0, &mut out)
            } else {
                CryptUnprotectData(&mut inb, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), 0, &mut out)
            }
        };
        if ok == 0 {
            return Err(RtError::Invalid("DPAPI call failed".into()));
        }
        let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
        unsafe { LocalFree(out.pbData as _) };
        Ok(v)
    }

    pub fn protect(k: &[u8]) -> Result<Vec<u8>> {
        call(k, true)
    }

    pub fn unprotect(b: &[u8]) -> Result<Vec<u8>> {
        call(b, false)
    }
}
