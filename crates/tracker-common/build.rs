//! Bakes the TrailFox license configuration into the binary at build time.
//! Set AGENTTRACKER_API_KEY / AGENTTRACKER_LICENSE_URL in the build environment (GitHub Actions
//! secrets/variables). When unset, clearly-marked placeholders are used so local builds still work.
fn main() {
    for (var, fallback) in [
        ("AGENTTRACKER_API_KEY", "UNSET-AGENTTRACKER-API-KEY"),
        ("AGENTTRACKER_LICENSE_URL", "https://license.invalid/agenttracker"),
    ] {
        println!("cargo:rerun-if-env-changed={var}");
        let value = std::env::var(var).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| fallback.to_string());
        println!("cargo:rustc-env={var}_BAKED={}", value.trim());
    }
}
