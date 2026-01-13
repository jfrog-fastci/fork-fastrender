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
    let build_stamp_path = build_dir.join("build.stamp");

    // If `build.rs` or any other input changes, Cargo will re-run this script. However, that does
    // *not* automatically invalidate artifacts we produced inside `OUT_DIR`.
    //
    // Keep a small stamp file keyed by the configuration we pass to libvpx so we only skip the
    // (expensive) configure+make steps when we're sure the existing `libvpx.a` matches.
    let configure_args: [&str; 11] = [
        "--target=generic-gnu",
        "--disable-examples",
        "--disable-tools",
        "--disable-unit-tests",
        "--disable-docs",
        "--enable-vp9",
        "--enable-vp8",
        "--disable-webm-io",
        "--enable-static",
        "--disable-shared",
        "--enable-pic",
        // NOTE: libvpx does not support `--disable-asm`. Using the generic target produces a
        // portable C-only build, avoiding `nasm`/`yasm` requirements in CI.
    ];
    let build_fingerprint = format!(
        "target={target}\nconfigure_args={}\n",
        configure_args.join(" ")
    );

    let existing_fingerprint_ok = match fs::read_to_string(&build_stamp_path) {
        Ok(s) => s == build_fingerprint,
        Err(_) => false,
    };

    if !(lib_path.exists() && existing_fingerprint_ok) {
        if build_dir.exists() {
            // If we got here then either the build output is missing/corrupt, or it was produced
            // with a different set of configure flags. Start fresh to avoid mixing objects from
            // incompatible configurations.
            fs::remove_dir_all(&build_dir).expect("remove stale libvpx build dir");
        }
        fs::create_dir_all(&build_dir).expect("create libvpx build dir");

        let configure = src_dir.join("configure");
        let mut configure_cmd = Command::new(configure);
        configure_cmd
            .current_dir(&build_dir)
            .args(configure_args);

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

        fs::write(&build_stamp_path, build_fingerprint)
            .expect("write libvpx build stamp file");
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
