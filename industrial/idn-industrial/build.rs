//! Windows: the IDN Node | Industrial icon (the IDN icon in the industrial orange,
//! installer/windows/industrial/recolour-icon.py) in its programs.
fn main() {
    println!("cargo:rerun-if-changed=assets/idn-industrial.rc");
    println!("cargo:rerun-if-changed=assets/idn-industrial.ico");
    // The `odbc` feature off Windows: where libodbc is, when it isn't the system's.
    println!("cargo:rerun-if-env-changed=IDN_ODBC_LIB");
    if let Ok(dir) = std::env::var("IDN_ODBC_LIB") {
        println!("cargo:rustc-link-search=native={dir}");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let _ = embed_resource::compile("assets/idn-industrial.rc", embed_resource::NONE);
    }
}
