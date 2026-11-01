// Embed LOKI's icon in the Windows executable; the tray loads it by ordinal 1.
fn main() {
    println!("cargo:rerun-if-changed=../../packaging/icon/loki.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("loki.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("compile the Windows icon resource");
    }
}
