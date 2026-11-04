//! Random tokens, HMAC request signing and constant-time comparison for the loopback-only
//! Local collector HTTP security: loopback-only, HMAC-signed tokens, strict origin checks.
use base64::Engine;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Signed requests older/newer than this (seconds) are rejected to limit replay.
pub const REPLAY_WINDOW_SEC: i64 = 300;
pub const HEADER_TIMESTAMP: &str = "x-at-timestamp";
pub const HEADER_SIGNATURE: &str = "x-at-signature";

pub fn token_urlsafe(nbytes: usize) -> String {
    let mut b = vec![0u8; nbytes];
    rand::thread_rng().fill_bytes(&mut b);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

pub fn random_hex(nbytes: usize) -> String {
    let mut b = vec![0u8; nbytes];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// `sha256=HMAC_SHA256(secret, "<ts>.<raw body>")`
pub fn sign_body(secret: &str, ts: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(ts.as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Validates a signature produced by [`sign_body`]; `now` is seconds since epoch.
pub fn verify_signature(secret: &str, ts: Option<&str>, sig: Option<&str>, body: &[u8], now: i64) -> Result<(), &'static str> {
    let (ts, sig) = match (ts.filter(|s| !s.is_empty()), sig.filter(|s| !s.is_empty())) {
        (Some(t), Some(s)) => (t, s),
        _ => return Err("signature required (X-AT-Timestamp and X-AT-Signature)"),
    };
    let ts_int: i64 = ts.trim().parse().map_err(|_| "invalid X-AT-Timestamp")?;
    if (now - ts_int).abs() > REPLAY_WINDOW_SEC {
        return Err("signature timestamp outside the replay window");
    }
    if !ct_eq(sign_body(secret, ts.trim(), body).as_bytes(), sig.trim().as_bytes()) {
        return Err("invalid signature");
    }
    Ok(())
}

/// Accepts only loopback Host headers for the given port (DNS-rebinding defence).
pub fn host_allowed(host: &str, port: u16) -> bool {
    [format!("127.0.0.1:{port}"), format!("localhost:{port}"), "127.0.0.1".into(), "localhost".into()]
        .iter()
        .any(|a| a == host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let s = sign_body("k", "100", b"{}");
        assert!(verify_signature("k", Some("100"), Some(&s), b"{}", 120).is_ok());
        assert!(verify_signature("k", Some("100"), Some(&s), b"{ }", 120).is_err());
        assert!(verify_signature("k", Some("100"), Some(&s), b"{}", 1000).is_err());
        assert!(verify_signature("x", Some("100"), Some(&s), b"{}", 120).is_err());
    }

    #[test]
    fn host_check() {
        assert!(host_allowed("127.0.0.1:5000", 5000));
        assert!(!host_allowed("evil.com", 5000));
    }
}
