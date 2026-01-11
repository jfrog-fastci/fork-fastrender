use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn has_cmd(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn find_cmd<'a>(candidates: &'a [&'a str]) -> Option<&'a str> {
    candidates.iter().copied().find(|c| has_cmd(c))
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("llvm-stackmaps should live at <workspace>/llvm-stackmaps")
        .to_path_buf()
}

#[cfg(target_os = "linux")]
#[test]
fn lld_can_link_stackmaps_section_with_explicit_range_symbols() {
    let Some(nm) = find_cmd(&["llvm-nm-18", "llvm-nm"]) else {
        // This test is specifically about lld + the linker script.
        return;
    };
    let Some(clang) = find_cmd(&["clang-18", "clang"]) else {
        eprintln!("skipping: clang not found in PATH (need clang-18 or clang)");
        return;
    };
    let Some(lld_fuse) = (if has_cmd("ld.lld-18") {
        Some("lld-18")
    } else if has_cmd("ld.lld") {
        Some("lld")
    } else {
        None
    }) else {
        eprintln!("skipping: lld not found in PATH (need ld.lld-18 or ld.lld)");
        return;
    };

    let ws_root = workspace_root();
    // This test emits a plain `.llvm_stackmaps` section (non-PIE layout). Use the
    // non-PIE linker script fragment that retains `.llvm_stackmaps` under
    // `--gc-sections` and defines `__start_llvm_stackmaps` / `__stop_llvm_stackmaps`.
    let stackmaps_ld = ws_root
        .join("runtime-native")
        .join("link")
        .join("stackmaps_nopie.ld");

    let tmp = tempfile::tempdir().expect("create tempdir");
    let project_dir = tmp.path();

    std::fs::create_dir(project_dir.join("src")).expect("create src dir");

    std::fs::write(
        project_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "stackmaps_test_bin"
version = "0.0.0"
edition = "2021"

[dependencies]
llvm-stackmaps = {{ path = "{}" }}
"#,
            ws_root.join("llvm-stackmaps").display()
        ),
    )
    .expect("write Cargo.toml");

    // Emit a `.llvm_stackmaps` section without any Rust references to it. With
    // `--gc-sections` enabled, the linker would drop the section unless the
    // linker script uses `KEEP(*(.llvm_stackmaps))`.
    std::fs::write(
        project_dir.join("src/main.rs"),
        r##"use std::arch::global_asm;

global_asm!(
    r#"
    .section .llvm_stackmaps,"a",@progbits
    .byte 0x01,0x02,0x03,0x04,0x05,0x06,0x07,0x08
    .previous
"#);

fn main() {
    let expected: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8];
    let bytes = llvm_stackmaps::stackmaps_bytes();
    assert_eq!(bytes, expected, "unexpected .llvm_stackmaps bytes");
}
"##,
    )
    .expect("write main.rs");

    let mut target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| ws_root.join("target"));
    if target_dir.is_relative() {
        target_dir = ws_root.join(target_dir);
    }
    // Avoid deadlocking on Cargo's target-dir lock: use a separate target dir from the outer
    // `cargo test` process, but keep it stable so the nested builds across tests can reuse artifacts.
    let target_dir = target_dir.join("llvm_stackmaps_nested_cargo_test_target");

    // Speed up the nested build: we only care about the final link result (symbols + section
    // retention), not debug info.
    let rustflags = format!(
        "-C debuginfo=0 -C linker={clang} -C link-arg=-fuse-ld={lld_fuse} -C link-arg=-no-pie -C link-arg=-Wl,-T,{} -C link-arg=-Wl,--gc-sections",
        stackmaps_ld.display()
    );

    let cargo_agent = ws_root.join("scripts").join("cargo_agent.sh");
    let status = Command::new("bash")
        .arg(cargo_agent)
        .arg("build")
        .arg("--offline")
        .arg("--quiet")
        .arg("--manifest-path")
        .arg(project_dir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("RUSTFLAGS", rustflags)
        .status()
        .expect("run nested build");
    assert!(status.success(), "nested build failed");

    let exe_path = target_dir.join("debug/stackmaps_test_bin");
    let status = Command::new(&exe_path)
        .status()
        .expect("run stackmaps_test_bin");
    assert!(status.success(), "stackmaps_test_bin failed");

    // Ensure the expected symbols were actually defined by the linker script.
    let nm_out = Command::new(nm)
        .arg(&exe_path)
        .output()
        .expect("run llvm-nm");
    assert!(nm_out.status.success(), "llvm-nm failed");

    let nm_stdout = String::from_utf8_lossy(&nm_out.stdout);
    assert!(
        nm_stdout.contains("__stackmaps_start"),
        "missing __stackmaps_start in output binary\n{nm_stdout}"
    );
    assert!(
        nm_stdout.contains("__stackmaps_end"),
        "missing __stackmaps_end in output binary\n{nm_stdout}"
    );

    // Also ensure the canonical start/stop symbols are exported (used by native-js/runtime-native
    // documentation and some tooling).
    assert!(
        nm_stdout.contains("__start_llvm_stackmaps"),
        "missing __start_llvm_stackmaps in output binary\n{nm_stdout}"
    );
    assert!(
        nm_stdout.contains("__stop_llvm_stackmaps"),
        "missing __stop_llvm_stackmaps in output binary\n{nm_stdout}"
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn lld_can_link_stackmaps_section_with_explicit_range_symbols() {}
