use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn has_cmd(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
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
    if !has_cmd("clang-18") || !has_cmd("ld.lld-18") || !has_cmd("llvm-nm") {
        // This test is specifically about lld + the linker script.
        return;
    }

    let ws_root = workspace_root();
    let stackmaps_ld = ws_root.join("runtime-native").join("link").join("stackmaps.ld");

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

    let target_dir = project_dir.join("target");

    let rustflags = format!(
        "-C linker=clang-18 -C link-arg=-fuse-ld=lld -C link-arg=-Wl,-T,{} -C link-arg=-Wl,--gc-sections",
        stackmaps_ld.display()
    );

    let status = Command::new("cargo")
        .arg("build")
        .arg("--quiet")
        .current_dir(project_dir)
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("RUSTFLAGS", rustflags)
        .status()
        .expect("run cargo build");
    assert!(status.success(), "cargo build failed");

    let exe_path = target_dir.join("debug/stackmaps_test_bin");
    let status = Command::new(&exe_path)
        .status()
        .expect("run stackmaps_test_bin");
    assert!(status.success(), "stackmaps_test_bin failed");

    // Ensure the expected symbols were actually defined by the linker script.
    let nm_out = Command::new("llvm-nm")
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
}

#[cfg(not(target_os = "linux"))]
#[test]
fn lld_can_link_stackmaps_section_with_explicit_range_symbols() {}
