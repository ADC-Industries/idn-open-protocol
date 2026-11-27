//! Generates Rust bindings to libobs's public C API from OBS Studio's headers and tells
//! Cargo where libobs is.
//!
//! - Linux: pkg-config finds `libobs` (Debian/Ubuntu: `libobs-dev`); the headers live in
//!   `<includedir>/obs`.
//! - Windows (built natively or cross-built from Linux with MinGW-w64):
//!   `IDN_OBS_INCLUDE` points at a directory holding an `obs/` directory with the
//!   headers (the obs-studio source's `libobs/`, plus `obs-frontend-api.h` and an
//!   `obsconfig.h`), and `IDN_OBS_LIB` at the directory holding `obs.dll` and
//!   `obs-frontend-api.dll` from the OBS release (MinGW links against DLLs directly;
//!   MSVC wants `obs.lib` / `obs-frontend-api.lib` import libraries there instead).
//!
//! Nothing from OBS is copied into this repository.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-env-changed=IDN_OBS_INCLUDE");
    println!("cargo:rerun-if-env-changed=IDN_OBS_LIB");
    println!("cargo:rerun-if-env-changed=IDN_OBS_CLANG_ARGS");
    let target = std::env::var("TARGET").unwrap_or_default();
    let windows = target.contains("windows");

    let (mut includes, lib_dir): (Vec<PathBuf>, Option<PathBuf>) =
        match std::env::var("IDN_OBS_INCLUDE") {
            Ok(inc) => (
                vec![PathBuf::from(&inc).join("obs"), PathBuf::from(inc)],
                std::env::var("IDN_OBS_LIB").ok().map(PathBuf::from),
            ),
            Err(_) => {
                let lib = match pkg_config::Config::new().probe("libobs") {
                    Ok(l) => l,
                    Err(e) => panic!(
                        "libobs not found ({e}). Install OBS Studio's development files \
                         (Debian/Ubuntu: `sudo apt install libobs-dev obs-studio`), or set \
                         IDN_OBS_INCLUDE and IDN_OBS_LIB, then build again."
                    ),
                };
                // Ubuntu's libobs.pc gives no usable -I flag.
                let dir = pkg_config::get_variable("libobs", "includedir")
                    .unwrap_or_else(|_| "/usr/include".into());
                let mut inc = lib.include_paths.clone();
                inc.push(PathBuf::from(&dir).join("obs"));
                inc.push(PathBuf::from(dir));
                (inc, None)
            }
        };
    if let Some(dir) = &lib_dir {
        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=dylib=obs");
        println!("cargo:rustc-link-lib=dylib=obs-frontend-api");
    }

    let mut b = bindgen::Builder::default()
        .header("wrapper.h")
        .allowlist_function("obs_.*")
        .allowlist_function("blog")
        .allowlist_function("bfree")
        .allowlist_function("video_output_.*")
        .allowlist_function("video_format_.*")
        .allowlist_function("audio_output_.*")
        .allowlist_function("os_gettime_ns")
        // The GPU source (§E.27): its textures shared with OBS's renderer, and its effect.
        .allowlist_function("gs_.*")
        .allowlist_type("obs_.*")
        .allowlist_type("video_.*")
        .allowlist_type("audio_.*")
        .allowlist_type("gs_.*")
        .allowlist_var("OBS_.*")
        .allowlist_var("LIBOBS_.*")
        .allowlist_var("LOG_.*")
        .allowlist_var("MAX_.*")
        .derive_default(true)
        .layout_tests(false);
    if windows {
        // Cross-building: clang parses the headers as the Windows target would, with
        // MinGW-w64's Windows headers (Debian/Ubuntu: `mingw-w64-x86-64-dev`).
        b = b
            .clang_arg("--target=x86_64-pc-windows-gnu")
            .clang_arg("-I/usr/x86_64-w64-mingw32/include")
            .clang_arg("-D_WIN32_WINNT=0x0A00");
        if let Ok(extra) = std::env::var("IDN_OBS_CLANG_ARGS") {
            for a in extra.split_whitespace() {
                b = b.clang_arg(a);
            }
        }
    }
    includes.dedup();
    for p in &includes {
        b = b.clang_arg(format!("-I{}", p.display()));
    }
    b = b.clang_arg("-DHAVE_OBSCONFIG_H");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    b.generate()
        .expect("bindgen over libobs headers")
        .write_to_file(out.join("bindings.rs"))
        .expect("writing bindings.rs");
}
