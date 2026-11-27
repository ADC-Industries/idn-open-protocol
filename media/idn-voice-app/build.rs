//! Windows: the IDN icon in idn-voice.exe (Explorer, the Start menu, its window, the tray).
fn main() {
    println!("cargo:rerun-if-changed=assets/voice.rc");
    println!("cargo:rerun-if-changed=assets/idn.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let _ = embed_resource::compile("assets/voice.rc", embed_resource::NONE);
    }
}
