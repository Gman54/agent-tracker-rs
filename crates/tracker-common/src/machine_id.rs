//! Stable, privacy-preserving machine fingerprint for Houndbit.
//! Only a SHA-256 hash is ever used; raw identifiers never leave the machine.
use sha2::{Digest, Sha256};

#[cfg(windows)]
fn primary_id() -> String {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY};
    winreg::RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(r"SOFTWARE\Microsoft\Cryptography", KEY_READ | KEY_WOW64_64KEY)
        .and_then(|k| k.get_value::<String, _>("MachineGuid"))
        .unwrap_or_default()
}

#[cfg(not(windows))]
fn primary_id() -> String {
    for p in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(v) = std::fs::read_to_string(p) {
            let v = v.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    String::new()
}

/// Product-scoped fingerprint for Houndbit.
pub fn fingerprint_from(primary: &str) -> String {
    let primary = if primary.is_empty() { format!("node:{}", machine_name()) } else { primary.to_string() };
    let s = format!("agenttracker-v1|{}|{}|{}", primary, std::env::consts::OS, std::env::consts::ARCH);
    hex::encode(Sha256::digest(s.as_bytes()))
}

pub fn machine_fingerprint() -> String {
    fingerprint_from(&primary_id())
}

pub fn machine_name() -> String {
    let n = hostname::get().ok().and_then(|h| h.into_string().ok()).unwrap_or_default();
    let n = if n.is_empty() { "unknown".to_string() } else { n };
    n.chars().take(64).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn stable_and_hex() {
        let a = super::fingerprint_from("abc");
        assert_eq!(a, super::fingerprint_from("abc"));
        assert_eq!(a.len(), 64);
    }
}
