//! Embeds Windows version info (company, product, description) into agenttracker-hook.exe.
//! Antivirus tools treat programs with no version info as more suspicious.
fn main() {
    println!("cargo:rerun-if-changed=hook.rc");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let _ = embed_resource::compile("hook.rc", embed_resource::NONE);
    }
}
