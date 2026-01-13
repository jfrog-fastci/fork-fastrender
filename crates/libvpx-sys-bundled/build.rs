use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // The libvpx sources are vendored, so changes are rare, but if they do happen we should
    // rebuild.
    println!("cargo:rerun-if-changed=upstream/libvpx/configure");

    let target = env::var("TARGET").expect("TARGET not set");
    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS not set");
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH not set");

    // This crate intentionally starts minimal: we only guarantee Linux x86_64 builds work.
    if target_os != "linux" || target_arch != "x86_64" {
        panic!(
            "libvpx-sys-bundled currently supports only Linux x86_64 (got target: {target})"
        );
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));
    let src_dir = manifest_dir.join("upstream").join("libvpx");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let build_dir = out_dir.join("libvpx-build");
    let lib_path = build_dir.join("libvpx.a");

    if !lib_path.exists() {
        fs::create_dir_all(&build_dir).expect("create libvpx build dir");

        let configure = src_dir.join("configure");
        let mut configure_cmd = Command::new(configure);
        configure_cmd
            .current_dir(&build_dir)
            // Build a portable C-only libvpx (no yasm/nasm dependency).
            .arg("--target=generic-gnu")
            // Minimize build output and dependencies.
            .arg("--disable-examples")
            .arg("--disable-tools")
            .arg("--disable-unit-tests")
            .arg("--disable-docs")
            // Ensure decode support for VP8/VP9 is built in.
            .arg("--enable-vp9")
            .arg("--enable-vp8")
            // Avoid libwebm dependency.
            .arg("--disable-webm-io")
            // We only need a static library for Rust linking.
            .arg("--enable-static")
            .arg("--disable-shared")
            // Allow linking into shared objects if the final Rust crate is a cdylib.
            .arg("--enable-pic");

        run(configure_cmd, "libvpx configure");

        let jobs = env::var("NUM_JOBS").unwrap_or_else(|_| "1".to_string());
        let mut make_cmd = Command::new("make");
        make_cmd.current_dir(&build_dir).arg(format!("-j{jobs}"));
        run(make_cmd, "libvpx make");

        if !lib_path.exists() {
            panic!(
                "libvpx build finished but {} was not found",
                lib_path.display()
            );
        }
    }

    println!("cargo:rustc-link-search=native={}", build_dir.display());
    println!("cargo:rustc-link-lib=static=vpx");
    // libvpx uses libm on Linux (e.g. floor, fabs). Some toolchains will pull it
    // in automatically, but make it explicit for static linking.
    println!("cargo:rustc-link-lib=m");
}

fn run(mut cmd: Command, desc: &str) {
    let status = cmd.status().unwrap_or_else(|e| panic!("failed to run {desc}: {e}"));
    if !status.success() {
        panic!("{desc} failed with status: {status}");
    }
}
