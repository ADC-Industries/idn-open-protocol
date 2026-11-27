//! Compiles the VMX codec (vendor/libvmx, MIT) into this crate. The AVX2 file alone gets
//! AVX2 flags: the codec picks that path at run time, so a machine without AVX2 still runs
//! the rest. The C++ runtime is linked statically on GNU targets (Windows has no
//! libstdc++-6.dll).

fn main() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/libvmx/src");
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    println!("cargo:rerun-if-changed={}", src.display());
    let base = |b: &mut cc::Build| {
        b.cpp(true)
            .cpp_link_stdlib(None)
            .std("c++17")
            .opt_level(3)
            .include(&src)
            .warnings(false)
            .flag_if_supported("-fno-exceptions")
            .define("NDEBUG", None);
        if os == "windows" {
            b.define("_WINDOWS", None);
        } else {
            // Its tables are declared with Microsoft's `__declspec(align(16))`: clang takes
            // that with -fdeclspec (as upstream builds it); GCC doesn't.
            b.compiler("clang++").flag("-fdeclspec");
        }
    };
    let mut files = vec!["vmxcodec.cpp"];
    // The AVX2 file's objects, compiled with its own flags, go in the one archive (the
    // rest calls into it).
    let mut objects = Vec::new();
    match arch.as_str() {
        "x86_64" => {
            files.push("vmxcodec_x86.cpp");
            let mut avx2 = cc::Build::new();
            base(&mut avx2);
            objects = avx2
                .file(src.join("vmxcodec_avx2.cpp"))
                .flag("-mavx2")
                .flag("-mbmi")
                .flag("-mbmi2")
                .flag("-mlzcnt")
                .compile_intermediates();
        }
        "aarch64" => files.push("vmxcodec_arm.cpp"),
        a => panic!("libvmx: no build for {a}"),
    }
    let mut b = cc::Build::new();
    base(&mut b);
    for o in objects {
        b.object(o);
    }
    if arch == "x86_64" {
        // The base path: SSE4.1, and the bit counting it uses (LZCNT, BMI1's TZCNT):
        // processors from about 2012 on.
        b.flag("-msse4.1")
            .flag("-mlzcnt")
            .flag("-mpopcnt")
            .flag("-mbmi");
    }
    for f in files {
        b.file(src.join(f));
    }
    b.compile("vmx");
    if env == "gnu" || env == "musl" {
        println!("cargo:rustc-link-lib=static:-bundle=stdc++");
        if os == "windows" {
            println!("cargo:rustc-link-lib=static:-bundle=winpthread");
        }
    }
}
