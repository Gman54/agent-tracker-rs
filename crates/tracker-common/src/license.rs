//! License stub. The API key and license server URL are baked in at build time by build.rs
//! (from the AGENTTRACKER_API_KEY / AGENTTRACKER_LICENSE_URL environment variables).
//!
//! TODO(license-server): wire real activation/validation against `license_url()` using
//! `api_key()` and `machine_id::machine_fingerprint()`. The initial build validates FORMAT ONLY.
use serde::Serialize;

const BAKED_API_KEY: &str = env!("AGENTTRACKER_API_KEY_BAKED");
const BAKED_LICENSE_URL: &str = env!("AGENTTRACKER_LICENSE_URL_BAKED");

/// Build-time API key for the license server. Never log or display this value.
pub fn api_key() -> &'static str {
    BAKED_API_KEY
}

pub fn license_url() -> &'static str {
    BAKED_LICENSE_URL
}

/// True when the build had real values (not the placeholders from build.rs).
pub fn is_configured() -> bool {
    !BAKED_API_KEY.starts_with("UNSET-") && !BAKED_LICENSE_URL.contains("license.invalid")
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LicenseState {
    /// No key entered yet.
    Missing,
    /// Key has a valid format; NOT verified with a server (stub).
    FormatValid,
    Invalid,
}

#[derive(Debug, Clone, Serialize)]
pub struct LicenseStatus {
    pub state: LicenseState,
    pub key_hint: Option<String>,
    pub server_verified: bool,
    pub build_configured: bool,
    pub message: String,
}

/// Expected key format: `AT-XXXXX-XXXXX-XXXXX-XXXXX` (A-Z, 0-9), case-insensitive.
pub fn validate_format(key: &str) -> bool {
    let k = key.trim().to_ascii_uppercase();
    let parts: Vec<&str> = k.split('-').collect();
    parts.len() == 5 && parts[0] == "AT" && parts[1..].iter().all(|p| p.len() == 5 && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

pub fn status_for(key: Option<&str>) -> LicenseStatus {
    let build_configured = is_configured();
    match key.map(str::trim).filter(|k| !k.is_empty()) {
        None => LicenseStatus {
            state: LicenseState::Missing,
            key_hint: None,
            server_verified: false,
            build_configured,
            message: "Enter your license key.".into(),
        },
        Some(k) if validate_format(k) => LicenseStatus {
            state: LicenseState::FormatValid,
            key_hint: Some(format!("AT-…{}", &k[k.len() - 5..].to_ascii_uppercase())),
            server_verified: false,
            build_configured,
            message: "Key format looks right. Online activation is not wired up in this build yet.".into(),
        },
        Some(_) => LicenseStatus {
            state: LicenseState::Invalid,
            key_hint: None,
            server_verified: false,
            build_configured,
            message: "That doesn't look like a Houndbit key (AT-XXXXX-XXXXX-XXXXX-XXXXX).".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format() {
        assert!(validate_format("AT-ABCDE-12345-FGHIJ-67890"));
        assert!(validate_format("at-abcde-12345-fghij-67890"));
        assert!(!validate_format("AT-ABCD-12345-FGHIJ-67890"));
        assert!(!validate_format("XX-ABCDE-12345-FGHIJ-67890"));
        assert_eq!(status_for(None).state, LicenseState::Missing);
        assert_eq!(status_for(Some("nope")).state, LicenseState::Invalid);
        assert!(!status_for(Some("AT-ABCDE-12345-FGHIJ-67890")).server_verified);
    }
}
