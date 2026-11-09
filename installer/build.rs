// Embeds the release PadMix.exe as the install payload, plus icon/version/manifest.
use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("..");
    let payload = std::env::var("PADMIX_PAYLOAD")
        .map(PathBuf::from)
        .unwrap_or_else(|_| root.join("target").join("release").join("padmix.exe"));
    let payload = payload.canonicalize().unwrap_or_else(|_| {
        panic!("{} not found: build PadMix first with `cargo build --release` (or run build-release.ps1)", payload.display())
    });
    println!("cargo:rustc-env=PADMIX_PAYLOAD={}", payload.display());
    println!("cargo:rerun-if-changed={}", payload.display());
    println!("cargo:rerun-if-env-changed=PADMIX_PAYLOAD");

    // Version comes from PadMix's own Cargo.toml so both always match.
    let cargo = std::fs::read_to_string(root.join("Cargo.toml")).expect("read ../Cargo.toml");
    let version = cargo
        .lines()
        .find_map(|l| l.strip_prefix("version = \"").and_then(|r| r.strip_suffix('"')))
        .expect("version in ../Cargo.toml")
        .to_string();
    println!("cargo:rustc-env=PADMIX_VERSION={version}");
    println!("cargo:rerun-if-changed=../Cargo.toml");

    let parts: Vec<&str> = version.split('.').collect();
    let defs = [
        format!("PADMIX_V1={}", parts.first().unwrap_or(&"0")),
        format!("PADMIX_V2={}", parts.get(1).unwrap_or(&"0")),
        format!("PADMIX_V3={}", parts.get(2).unwrap_or(&"0")),
        format!("PADMIX_VSTR=\"{version}\""),
        "PADMIX_DESC=\"PadMix Setup\"".to_string(),
        "PADMIX_INTERNAL=\"PadMix-Setup\"".to_string(),
        "PADMIX_FILE=\"PadMix-Setup.exe\"".to_string(),
    ];
    println!("cargo:rerun-if-changed=../assets/padmix.rc");
    embed_resource::compile("../assets/padmix.rc", defs.iter()).manifest_optional().unwrap();
}
