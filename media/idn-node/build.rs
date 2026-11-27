//! Windows: the IDN icon in idn-node.exe and idn-session.exe (Explorer, the Start menu, the
//! installed programs list).
fn main() {
    println!("cargo:rerun-if-changed=assets/idn.rc");
    println!("cargo:rerun-if-changed=assets/idn.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let _ = embed_resource::compile("assets/idn.rc", embed_resource::NONE);
    }
}
