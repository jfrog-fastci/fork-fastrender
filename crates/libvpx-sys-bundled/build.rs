use std::collections::hash_map::DefaultHasher;
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Tool discovery depends on PATH (and PATHEXT on Windows). Track these so changes to the
    // environment (e.g. installing `gmake` on macOS) reliably re-run the build script.
    println!("cargo:rerun-if-env-changed=PATH");
    println!("cargo:rerun-if-env-changed=PATHEXT");

    let target = env::var("TARGET").expect("TARGET not set");
    let host = env::var("HOST").expect("HOST not set");
    let target_key = target.replace('-', "_");

    // Re-run if the toolchain environment changes. These vars are honored by libvpx's configure
    // script and affect the produced `libvpx.a`.
    for var in ["CC", "CXX", "CFLAGS", "AR", "AS", "LD", "CROSS", "MAKE"] {
        println!("cargo:rerun-if-env-changed={var}");
        println!("cargo:rerun-if-env-changed={var}_{target_key}");
    }
    // Used to infer the libvpx Visual Studio toolchain (vsNN) when targeting MSVC.
    println!("cargo:rerun-if-env-changed=VisualStudioVersion");

    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS not set");
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH not set");
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();

    // Map Rust target triples to libvpx's configure toolchain names.
    //
    // We intentionally build libvpx without yasm/nasm assembly by default to keep builds
    // portable/CI-friendly.
    let is_msvc_target = target_os == "windows" && target_env == "msvc";

    let libvpx_toolchain = match (target_os.as_str(), target_arch.as_str(), target_env.as_str()) {
        ("linux", "x86_64", "gnu") => "generic-gnu".to_string(),
        ("linux", "x86_64", other) => unsupported(
            &target,
            &host,
            &format!(
                "Linux x86_64 target is supported only for the GNU environment (x86_64-unknown-linux-gnu). \
Got env={other:?}. musl targets are not supported by the bundled libvpx build yet; link against a system libvpx or use a GNU target."
            ),
        ),
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
        ("macos", "aarch64", _) => {
            // Apple Silicon. libvpx uses `arm64` in its toolchain naming.
            if !host.contains("apple-darwin") {
                unsupported(
                    &target,
                    &host,
                    "macOS target requested but build host is not macOS. Cross-compiling the bundled libvpx is not supported; build on macOS or provide a prebuilt libvpx.",
                );
            }
            // arm64 support starts at darwin20 (macOS 11). Clamp to a known toolchain name.
            let darwin = detect_darwin_major().unwrap_or(22).max(20);
            format!("arm64-darwin{darwin}-gcc")
        }
        ("windows", "x86", "gnu") => "x86-win32-gcc".to_string(),
        ("windows", "x86_64", "gnu") => "x86_64-win64-gcc".to_string(),
        ("windows", "aarch64", "gnu") => "arm64-win64-gcc".to_string(),
        ("windows", "x86", "msvc") => {
            if !host.contains("windows") {
                unsupported(
                    &target,
                    &host,
                    "Windows MSVC target requested but build host is not Windows. \
Cross-compiling the bundled libvpx for MSVC is not supported; build on Windows or link against a system libvpx.",
                );
            }
            let vs_ver = detect_visual_studio_major().unwrap_or(16);
            format!("x86-win32-vs{vs_ver}")
        }
        ("windows", "x86_64", "msvc") => {
            if !host.contains("windows") {
                unsupported(
                    &target,
                    &host,
                    "Windows MSVC target requested but build host is not Windows. \
Cross-compiling the bundled libvpx for MSVC is not supported; build on Windows or link against a system libvpx.",
                );
            }
            let vs_ver = detect_visual_studio_major().unwrap_or(16);
            format!("x86_64-win64-vs{vs_ver}")
        }
        ("windows", "aarch64", "msvc") => {
            if !host.contains("windows") {
                unsupported(
                    &target,
                    &host,
                    "Windows MSVC target requested but build host is not Windows. \
Cross-compiling the bundled libvpx for MSVC is not supported; build on Windows or link against a system libvpx.",
                );
            }
            let vs_ver = detect_visual_studio_major().unwrap_or(16);
            if vs_ver < 15 {
                unsupported(
                    &target,
                    &host,
                    "Windows ARM64 MSVC builds require Visual Studio 2017+ (vs15) so that the generated projects can target ARM64.",
                );
            }
            format!("arm64-win64-vs{vs_ver}")
        }
        _ => unsupported(
            &target,
            &host,
            "unsupported target for bundled libvpx build. Supported targets: linux x86_64-gnu, macOS x86_64/aarch64, Windows x86-gnu (MinGW), Windows x86_64-gnu (MinGW), Windows aarch64-gnu (MinGW), Windows x86-msvc (best-effort), Windows x86_64-msvc (best-effort), Windows aarch64-msvc (best-effort).",
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
    // For MSVC we copy the produced `.lib` output to a stable location (`vpx.lib`) so downstream
    // linking can always use `-lvpx`.
    let lib_path = if is_msvc_target {
        build_dir.join("vpx.lib")
    } else {
        build_dir.join("libvpx.a")
    };
    let build_stamp_path = build_dir.join("build.stamp");

    // If `build.rs` or any other input changes, Cargo will re-run this script. However, that does
    // *not* automatically invalidate artifacts we produced inside `OUT_DIR`.
    //
    // Keep a small stamp file keyed by the configuration we pass to libvpx so we only skip the
    // (expensive) configure+make steps when we're sure the existing `libvpx.a` matches.
    let mut configure_args = Vec::<String>::new();
    configure_args.push(format!("--target={libvpx_toolchain}"));
    // Visual Studio builds go through libvpx's generated `.sln` + `msbuild` flow.
    // Force external_build so configure doesn't require a GCC-like toolchain for probing.
    if is_msvc_target {
        configure_args.push("--enable-external-build".to_string());
        // libvpx's VCXProj generator only accepts -I/-D/-L/-l style flags. If optimizations are
        // enabled, configure injects `-O3` into CFLAGS, which breaks project generation.
        //
        // MSVC Release builds still use optimized compiler settings via the generated VC projects,
        // so disable configure's generic `-O` flags here.
        configure_args.push("--disable-optimizations".to_string());
    }
    for arg in disable_yasm_nasm_by_default(target_arch.as_str()) {
        configure_args.push(arg.to_string());
    }
    for arg in [
        "--disable-examples",
        "--disable-tools",
        "--disable-unit-tests",
        "--disable-docs",
        // We only need decoding for the browser/video pipeline; avoid building encoder
        // code (and C++ sources) by default.
        "--enable-vp8-decoder",
        "--enable-vp9-decoder",
        "--enable-vp9-highbitdepth",
        "--disable-vp8-encoder",
        "--disable-vp9-encoder",
        "--disable-webm-io",
        // Avoid pulling in / building libyuv (and its C++ compilation) for the minimal VP9 decode
        // use-case.
        "--disable-libyuv",
        "--enable-static",
        "--disable-shared",
        "--enable-pic",
    ] {
        configure_args.push(arg.to_string());
    }

    // libvpx uses various toolchain env vars; compute the effective values that will be seen by
    // its configure script.
    let mut cc = get_scoped_env("CC", &target_key);
    let mut cxx = get_scoped_env("CXX", &target_key);
    let cflags = get_scoped_env("CFLAGS", &target_key);
    let mut ar = get_scoped_env("AR", &target_key);
    let make_env = get_scoped_env("MAKE", &target_key);
    let as_env = get_scoped_env("AS", &target_key);
    let mut ld = get_scoped_env("LD", &target_key);
    let cross_env = get_scoped_env("CROSS", &target_key);

    // Default to the clang toolchain when building for native Unix targets. The repository already
    // requires clang for linking (via `.cargo/config.toml`), but minimal CI containers may not
    // ship GCC.
    if cc.is_empty() && target_os != "windows" {
        cc = "clang".to_string();
    }
    if cxx.is_empty() && target_os != "windows" {
        // We build a decoder-only libvpx with libyuv disabled, so a dedicated C++ driver isn't
        // required. Using the same tool as `CC` is more likely to be present in minimal
        // environments.
        cxx = cc.clone();
    }
    if ld.is_empty() && target_os != "windows" {
        // Some minimal CI environments don't have `gcc`, but they do have `clang` (required by this
        // repo's `.cargo/config.toml` linker setting). Configure uses `LD` for link tests, so set
        // it explicitly.
        ld = cc.clone();
    }
    if ar.is_empty() && target_os != "windows" {
        ar = if tool_in_path("llvm-ar") {
            "llvm-ar".to_string()
        } else {
            "ar".to_string()
        };
    }

    // When cross-compiling for MinGW targets from a non-Windows host, libvpx typically needs
    // `CROSS=<prefix>-` to find the binutils toolchain. Use a sensible default if the user hasn't
    // provided their own tool variables.
    let needs_default_cross = target_os == "windows"
        && target_env == "gnu"
        && !host.contains("windows")
        && cross_env.is_empty()
        && cc.is_empty();
    let effective_cross = if needs_default_cross {
        default_mingw_cross_prefix(&target, &host, target_arch.as_str())
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

    let effective_make = if make_env.is_empty() {
        // macOS ships BSD make as `make`; libvpx's build requires GNU make. Prefer `gmake` if
        // present, otherwise fall back to `make`.
        if tool_in_path("gmake") {
            "gmake".to_string()
        } else {
            "make".to_string()
        }
    } else {
        make_env.clone()
    };

    // libvpx's build uses GNU-make specific features. On macOS the system `make` is BSD make, which
    // will fail with confusing errors ("missing separator", etc). Detect this early and emit a
    // clear hint.
    if target_os == "macos" && !is_gnu_make(&effective_make) {
        unsupported(
            &target,
            &host,
            "libvpx build requires GNU make. Install GNU make (e.g. `brew install make`) and ensure `gmake` is in PATH, or set `MAKE=gmake`.",
        );
    }

    let source_tree_hash = hash_dir_contents(&src_dir);
    let build_fingerprint = format!(
        "target={target}\nhost={host}\ncc={cc}\ncxx={cxx}\nld={ld}\ncflags={cflags}\nar={ar}\nmake={effective_make}\nas={effective_as}\ncross={effective_cross}\nlibvpx_source_tree_hash={source_tree_hash}\nconfigure_args={}\n",
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

        // `configure` is a POSIX shell script. On Windows hosts, invoke it via
        // `sh`/`bash` (as required by libvpx's upstream README).
        let mut configure_cmd = if host.contains("windows") {
            let shell = find_windows_posix_shell(&target, &host);
            let mut cmd = Command::new(shell);
            cmd.arg(&configure_src_path);
            cmd
        } else {
            Command::new(&configure_src_path)
        };
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
        if !ld.is_empty() {
            configure_cmd.env("LD", &ld);
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
        if is_msvc_target {
            // MSVC builds:
            // 1) Run `make` to generate the VCXProj + solution files, without building every
            //    configuration by default (it would otherwise build both Debug+Release).
            // 2) Build only the Release|x64 configuration via the generated makefile target (which
            //    calls `msbuild.exe`).
            // 3) Copy the produced `.lib` to `vpx.lib` for consistent downstream linking.
            let mut make_gen_cmd = Command::new(&effective_make);
            make_gen_cmd
                .current_dir(&build_dir)
                .arg(format!("-j{jobs}"))
                .env("NO_LAUNCH_DEVENV", "1");
            run(make_gen_cmd, "libvpx make (msvc generate projects)");

            // Now build only the desired Release|<Platform> configuration.
            let msvc_cfg = match target_arch.as_str() {
                "x86" => "Release_Win32",
                "x86_64" => "Release_x64",
                "aarch64" => "Release_ARM64",
                other => unsupported(
                    &target,
                    &host,
                    &format!(
                        "unsupported Windows MSVC arch {other:?} for bundled libvpx build (expected x86_64 or aarch64)"
                    ),
                ),
            };
            let mut make_build_cmd = Command::new(&effective_make);
            make_build_cmd
                .current_dir(&build_dir)
                .arg(format!("-j{jobs}"))
                .arg("target=solution")
                .arg(msvc_cfg);
            run(
                make_build_cmd,
                &format!("libvpx make (msvc build {msvc_cfg})"),
            );

            let produced = find_msvc_static_lib(&build_dir).unwrap_or_else(|| {
                panic!(
                    "libvpx MSVC build finished, but no static .lib was found under {}. \
Ensure MSYS2/Cygwin `make` is installed and `msbuild.exe` is available in PATH (Visual Studio Build Tools / Developer Command Prompt).",
                    build_dir.display()
                )
            });
            fs::copy(&produced, &lib_path).unwrap_or_else(|e| {
                panic!(
                    "failed to copy built libvpx MSVC library from {} to {}: {e}",
                    produced.display(),
                    lib_path.display()
                )
            });
        } else {
            let mut make_cmd = Command::new(&effective_make);
            make_cmd
                .current_dir(&build_dir)
                .arg(format!("-j{jobs}"))
                // Only build the primary static library we link against.
                .arg("libvpx.a");
            run(make_cmd, "libvpx make");
        }

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

fn find_windows_posix_shell(target: &str, host: &str) -> String {
    for candidate in ["sh", "bash"] {
        match Command::new(candidate).arg("-c").arg("exit 0").status() {
            Ok(status) if status.success() => return candidate.to_string(),
            _ => continue,
        }
    }

    unsupported(
        target,
        host,
        "Windows builds of bundled libvpx require a POSIX shell (`sh`/`bash`) in PATH. \
Install MSYS2 or Cygwin and ensure the shell is available.",
    );
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

fn default_mingw_cross_prefix(target: &str, host: &str, target_arch: &str) -> String {
    // The canonical MinGW-w64 toolchain prefixes used by many Linux distributions.
    match target_arch {
        "x86" => "i686-w64-mingw32-".to_string(),
        "x86_64" => "x86_64-w64-mingw32-".to_string(),
        "aarch64" => "aarch64-w64-mingw32-".to_string(),
        other => unsupported(
            target,
            host,
            &format!(
                "unsupported MinGW arch {other:?} for bundled libvpx build (set CROSS explicitly if you have a different toolchain prefix)"
            ),
        ),
    }
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

fn detect_visual_studio_major() -> Option<u32> {
    // On a VS Developer Command Prompt this is typically set to e.g. `17.0`.
    let ver = env::var("VisualStudioVersion").ok()?;
    let major = ver.split('.').next()?.parse::<u32>().ok()?;
    if (14..=17).contains(&major) {
        Some(major)
    } else {
        None
    }
}

fn find_msvc_static_lib(build_dir: &Path) -> Option<PathBuf> {
    // Prefer the common MSVC static lib names produced by libvpx.
    // vpxmd.lib: dynamic CRT, vpxmt.lib: static CRT.
    let preferred_names = ["vpxmd.lib", "vpxmt.lib", "vpx.lib"];

    let mut files = Vec::new();
    collect_files(build_dir, build_dir, &mut files);

    let mut best: Option<(i32, PathBuf)> = None;
    for rel in files {
        let file_name = rel.file_name()?.to_string_lossy();
        if !preferred_names.iter().any(|n| *n == file_name) {
            continue;
        }

        // Score paths: prefer x64/Release over Debug/Win32, etc.
        let mut score = 0;
            for comp in rel.components() {
                let comp = comp.as_os_str().to_string_lossy();
                match comp.as_ref() {
                    "x64" | "amd64" => score += 20,
                    "ARM64" | "arm64" => score += 20,
                    "Release" => score += 10,
                    "Win32" => score -= 10,
                    "Debug" => score -= 5,
                    _ => {}
                }
            }
        if score <= 0 {
            // Still accept it, but prefer release-ish outputs.
            score += 1;
        }

        let abs = build_dir.join(&rel);
        match &mut best {
            Some((best_score, best_path)) => {
                if score > *best_score {
                    *best_score = score;
                    *best_path = abs;
                }
            }
            None => best = Some((score, abs)),
        }
    }

    best.map(|(_, p)| p)
}

fn tool_in_path(tool: &str) -> bool {
    let path = match env::var_os("PATH") {
        Some(p) => p,
        None => return false,
    };
    // Windows uses PATHEXT to find executables when the extension is omitted (e.g. `make.exe` when
    // invoked as `make`). `Path::exists` does not apply PATHEXT, so emulate it here for robustness.
    let pathext = if cfg!(windows) {
        env::var_os("PATHEXT")
            .and_then(|s| s.into_string().ok())
            .map(|s| {
                s.split(';')
                    .filter(|ext| !ext.is_empty())
                    .map(|ext| ext.to_ascii_lowercase())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec![".exe".to_string(), ".cmd".to_string(), ".bat".to_string()])
    } else {
        Vec::new()
    };

    let has_ext = Path::new(tool).extension().is_some();
    for dir in env::split_paths(&path) {
        if dir.join(tool).exists() {
            return true;
        }
        if cfg!(windows) && !has_ext {
            for ext in &pathext {
                if dir.join(format!("{tool}{ext}")).exists() {
                    return true;
                }
            }
        }
    }
    false
}

fn is_gnu_make(make: &str) -> bool {
    let out = Command::new(make).arg("--version").output();
    match out {
        Ok(out) => {
            let mut combined = Vec::new();
            combined.extend_from_slice(&out.stdout);
            combined.extend_from_slice(&out.stderr);
            let text = String::from_utf8_lossy(&combined);
            text.contains("GNU Make")
        }
        Err(_) => false,
    }
}
