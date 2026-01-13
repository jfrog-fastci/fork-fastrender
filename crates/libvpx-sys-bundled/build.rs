use std::collections::hash_map::DefaultHasher;
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let target = env::var("TARGET").expect("TARGET not set");
    let host = env::var("HOST").expect("HOST not set");
    let target_key = target.replace('-', "_");

    // Re-run if the toolchain environment changes. These vars are honored by libvpx's configure
    // script and affect the produced `libvpx.a`.
    for var in ["CC", "CXX", "CFLAGS", "AR", "AS", "CROSS"] {
        println!("cargo:rerun-if-env-changed={var}");
        println!("cargo:rerun-if-env-changed={var}_{target_key}");
    }

    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS not set");
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH not set");
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();

    // Map Rust target triples to libvpx's configure toolchain names.
    //
    // We intentionally build libvpx without yasm/nasm assembly by default to keep builds
    // portable/CI-friendly.
    let libvpx_toolchain = match (target_os.as_str(), target_arch.as_str(), target_env.as_str()) {
        ("linux", "x86_64", _) => "generic-gnu".to_string(),
        ("macos", "x86_64", _) => {
            // libvpx's configure expects a Darwin kernel version suffix (darwinXX). Use the host's
            // Darwin version when available, clamped to the versions known by this vendored
            // libvpx release.
            if !host.contains("apple-darwin") {
                unsupported(
                    &target,
                    &host,
                    "macOS target requested but build host is not macOS. Cross-compiling the bundled libvpx is not supported; build on macOS or provide a prebuilt libvpx.",
                );
            }
            format!("x86_64-darwin{}-gcc", detect_darwin_major().unwrap_or(22))
        }
        ("windows", "x86_64", "gnu") => "x86_64-win64-gcc".to_string(),
        ("windows", "x86_64", "msvc") => {
            unsupported(
                &target,
                &host,
                "Windows MSVC targets are not supported by the bundled libvpx build yet. \
Try using the MinGW target (`--target x86_64-pc-windows-gnu`) with an MSYS2/Cygwin environment. \
If you want to experiment with MSVC, libvpx supports VS toolchains like `--target=x86_64-win64-vs16`, \
but this crate's build script does not currently invoke that flow. \
Alternatively, link against a system-provided libvpx.",
            );
        }
        _ => unsupported(
            &target,
            &host,
            "unsupported target for bundled libvpx build. Supported targets: linux x86_64, macOS x86_64, Windows x86_64-gnu (MinGW).",
        ),
    };

    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));
    let src_dir = manifest_dir.join("upstream").join("libvpx");
    // Make sure Cargo reruns this build script when any vendored file changes. Note: `rerun-if-changed`
    // on a directory is not guaranteed to be recursive, so list all files explicitly.
    emit_rerun_if_changed_recursively(&src_dir);
    let configure_src_path = src_dir.join("configure");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let build_dir = out_dir.join("libvpx-build");
    let lib_path = build_dir.join("libvpx.a");
    let build_stamp_path = build_dir.join("build.stamp");

    // If `build.rs` or any other input changes, Cargo will re-run this script. However, that does
    // *not* automatically invalidate artifacts we produced inside `OUT_DIR`.
    //
    // Keep a small stamp file keyed by the configuration we pass to libvpx so we only skip the
    // (expensive) configure+make steps when we're sure the existing `libvpx.a` matches.
    let mut configure_args = Vec::<String>::new();
    configure_args.push(format!("--target={libvpx_toolchain}"));
    for arg in disable_yasm_nasm_by_default(target_arch.as_str()) {
        configure_args.push(arg.to_string());
    }
    for arg in [
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
    ] {
        configure_args.push(arg.to_string());
    }

    // libvpx uses various toolchain env vars; compute the effective values that will be seen by
    // its configure script.
    let cc = get_scoped_env("CC", &target_key);
    let cxx = get_scoped_env("CXX", &target_key);
    let cflags = get_scoped_env("CFLAGS", &target_key);
    let ar = get_scoped_env("AR", &target_key);
    let as_env = get_scoped_env("AS", &target_key);
    let cross_env = get_scoped_env("CROSS", &target_key);

    // When cross-compiling for MinGW targets from a non-Windows host, libvpx typically needs
    // `CROSS=<prefix>-` to find the binutils toolchain. Use a sensible default if the user hasn't
    // provided their own tool variables.
    let needs_default_cross = target_os == "windows"
        && target_env == "gnu"
        && !host.contains("windows")
        && cross_env.is_empty()
        && cc.is_empty();
    let effective_cross = if needs_default_cross {
        // The canonical MinGW-w64 prefix used by most Linux distributions.
        "x86_64-w64-mingw32-".to_string()
    } else {
        cross_env
    };

    // By default we build without yasm/nasm assembly for portability. libvpx's x86/x86_64
    // toolchains normally require yasm/nasm and will error out during configure if none is found.
    // Setting `AS` to any non-empty value bypasses that detection; we use `true` (a harmless
    // no-op) and ensure no `.asm` sources are enabled via configure flags.
    let effective_as = if matches!(target_arch.as_str(), "x86" | "x86_64") && as_env.is_empty() {
        "true".to_string()
    } else {
        as_env
    };

    let source_tree_hash = hash_dir_contents(&src_dir);
    let build_fingerprint = format!(
        "target={target}\nhost={host}\ncc={cc}\ncxx={cxx}\ncflags={cflags}\nar={ar}\nas={effective_as}\ncross={effective_cross}\nlibvpx_source_tree_hash={source_tree_hash}\nconfigure_args={}\n",
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

        let mut configure_cmd = Command::new(&configure_src_path);
        configure_cmd.current_dir(&build_dir);

        // Ensure libvpx's configure sees the target-scoped tool variables that Cargo/cc-rs
        // commonly use (e.g. `CC_x86_64_pc_windows_gnu`).
        //
        // We set these explicitly (rather than relying on inheriting the parent environment) so
        // the build is correct even when only scoped vars are provided.
        if !cc.is_empty() {
            configure_cmd.env("CC", &cc);
        }
        if !cxx.is_empty() {
            configure_cmd.env("CXX", &cxx);
        }
        if !cflags.is_empty() {
            configure_cmd.env("CFLAGS", &cflags);
        }
        if !ar.is_empty() {
            configure_cmd.env("AR", &ar);
        }
        if !effective_cross.is_empty() {
            configure_cmd.env("CROSS", &effective_cross);
        }
        if !effective_as.is_empty() {
            configure_cmd.env("AS", &effective_as);
        }

        configure_cmd.args(&configure_args);
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

        fs::write(&build_stamp_path, build_fingerprint).expect("write libvpx build stamp file");
    }

    println!("cargo:rustc-link-search=native={}", build_dir.display());
    println!("cargo:rustc-link-lib=static=vpx");
    if target_os == "linux" {
        // libvpx uses libm on Linux (e.g. floor, fabs). Some toolchains will pull it
        // in automatically, but make it explicit for static linking.
        println!("cargo:rustc-link-lib=m");
    }
}

fn run(mut cmd: Command, desc: &str) {
    let status = cmd.status().unwrap_or_else(|e| panic!("failed to run {desc}: {e}"));
    if !status.success() {
        panic!("{desc} failed with status: {status}");
    }
}

fn hash_dir_contents(root: &Path) -> u64 {
    // Hash all files in the directory recursively, in a stable path order, to produce a
    // deterministic fingerprint of the vendored libvpx sources.
    let mut paths = Vec::new();
    collect_files(root, root, &mut paths);
    paths.sort();

    let mut hasher = DefaultHasher::new();
    for rel_path in paths {
        rel_path.hash(&mut hasher);
        let abs = root.join(&rel_path);
        let bytes =
            fs::read(&abs).unwrap_or_else(|e| panic!("failed to read {}: {e}", abs.display()));
        bytes.hash(&mut hasher);
    }
    hasher.finish()
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|e| panic!("failed to read dir {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| {
            panic!("failed to read dir entry under {}: {e}", dir.display())
        });
        let path = entry.path();
        let ty = entry.file_type().unwrap_or_else(|e| {
            panic!("failed to get file type for {}: {e}", path.display())
        });
        if ty.is_dir() {
            collect_files(root, &path, out);
        } else if ty.is_file() || ty.is_symlink() {
            let rel = path.strip_prefix(root).expect("strip_prefix").to_path_buf();
            out.push(rel);
        }
    }
}

fn emit_rerun_if_changed_recursively(src_dir: &Path) {
    let mut files = Vec::new();
    collect_files(src_dir, src_dir, &mut files);
    files.sort();
    for rel in files {
        let rel_str = rel
            .to_str()
            .unwrap_or_else(|| panic!("non-utf8 path under {}: {}", src_dir.display(), rel.display()));
        println!("cargo:rerun-if-changed=upstream/libvpx/{rel_str}");
    }
}

fn unsupported(target: &str, host: &str, msg: &str) -> ! {
    println!("cargo:warning=libvpx-sys-bundled: {msg} (target={target}, host={host})");
    panic!("libvpx-sys-bundled: {msg} (target={target}, host={host})");
}

fn get_scoped_env(var: &str, target_key: &str) -> String {
    env::var(var)
        .ok()
        .or_else(|| env::var(format!("{var}_{target_key}")).ok())
        .unwrap_or_default()
}

fn disable_yasm_nasm_by_default(target_arch: &str) -> Vec<&'static str> {
    if !matches!(target_arch, "x86" | "x86_64") {
        return Vec::new();
    }
    vec![
        "--disable-runtime-cpu-detect",
        "--disable-mmx",
        "--disable-sse",
        "--disable-sse2",
        "--disable-sse3",
        "--disable-ssse3",
        "--disable-sse4_1",
        "--disable-avx",
        "--disable-avx2",
        "--disable-avx512",
    ]
}

fn detect_darwin_major() -> Option<u32> {
    let out = Command::new("uname").arg("-r").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let major: u32 = s.split('.').next()?.trim().parse().ok()?;

    // This vendored libvpx release's configure knows darwin9..darwin22. Clamp to that range so
    // building on newer macOS versions doesn't fail with "Unrecognized toolchain".
    let clamped = major.clamp(9, 22);
    if clamped != major {
        println!(
            "cargo:warning=libvpx-sys-bundled: host Darwin {major} is newer than this vendored libvpx; using darwin{clamped} toolchain"
        );
    }
    Some(clamped)
}
