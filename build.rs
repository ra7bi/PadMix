// Embed the icon and version info into padmix.exe (needs rc.exe from the Windows SDK).
fn main() {
    println!("cargo:rerun-if-changed=assets/padmix.rc");
    println!("cargo:rerun-if-changed=assets/padmix.ico");
    println!("cargo:rerun-if-changed=assets/padmix.manifest");
    let v1 = std::env::var("CARGO_PKG_VERSION_MAJOR").unwrap();
    let v2 = std::env::var("CARGO_PKG_VERSION_MINOR").unwrap();
    let v3 = std::env::var("CARGO_PKG_VERSION_PATCH").unwrap();
    let vstr = std::env::var("CARGO_PKG_VERSION").unwrap();
    let defs = [
        format!("PADMIX_V1={v1}"),
        format!("PADMIX_V2={v2}"),
        format!("PADMIX_V3={v3}"),
        format!("PADMIX_VSTR=\"{vstr}\""),
        "PADMIX_DESC=\"PadMix soundboard\"".to_string(),
        "PADMIX_INTERNAL=\"PadMix\"".to_string(),
        "PADMIX_FILE=\"PadMix.exe\"".to_string(),
    ];
    embed_resource::compile("assets/padmix.rc", defs.iter()).manifest_optional().unwrap();
}
