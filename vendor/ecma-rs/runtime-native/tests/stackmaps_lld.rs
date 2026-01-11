use std::path::PathBuf;
use std::process::Command;

fn has_cmd(cmd: &str) -> bool {
  Command::new(cmd)
    .arg("--version")
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .is_ok()
}

#[cfg(target_os = "linux")]
#[test]
fn lld_linker_script_defines_stackmaps_range_symbols() {
  if !has_cmd("clang-18") || !has_cmd("ld.lld-18") || !has_cmd("llvm-nm") {
    // Allow building in minimal environments; this test is specifically about
    // lld + linker-script behavior.
    return;
  }

  let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let stackmaps_ld = crate_dir.join("link/stackmaps.ld");

  let temp = tempfile::tempdir().expect("create tempdir");
  let project_dir = temp.path();

  std::fs::create_dir(project_dir.join("src")).expect("create src dir");

  std::fs::write(
    project_dir.join("Cargo.toml"),
    format!(
      r#"[package]
name = "stackmaps_test_bin"
version = "0.0.0"
edition = "2021"

[dependencies]
runtime-native = {{ path = "{}", features = ["llvm_stackmaps_linker"] }}
"#,
      crate_dir.display()
    ),
  )
  .expect("write Cargo.toml");

  // Define `.llvm_stackmaps` bytes without creating any Rust-level references to
  // them. With `--gc-sections` enabled, the linker would drop the section unless
  // our linker script uses `KEEP(*(.llvm_stackmaps))`.
  std::fs::write(
    project_dir.join("src/main.rs"),
    r##"use std::arch::global_asm;

global_asm!(
    r#"
    .section .llvm_stackmaps,"a",@progbits
    // Minimal LLVM StackMap v3 header (16 bytes):
    //   u8  version = 3
    //   u8  reserved0 = 0
    //   u16 reserved1 = 0
    //   u32 num_functions = 0
    //   u32 num_constants = 0
    //   u32 num_records = 0
    .byte 0x03,0x00,0x00,0x00, 0x00,0x00,0x00,0x00, 0x00,0x00,0x00,0x00, 0x00,0x00,0x00,0x00
    .previous
"#);

fn main() {
    let stackmaps = runtime_native::try_load_via_linker_symbols()
        .expect("expected linker-defined stackmap boundary symbols");
    assert!(!stackmaps.is_empty(), "expected non-empty .llvm_stackmaps bytes");

    let blobs = runtime_native::stackmap_loader::parse_stackmap_blobs(stackmaps)
        .expect("expected StackMap v3 header");
    assert!(!blobs.is_empty(), "expected at least one stackmap blob");
    assert_eq!(blobs[0].version, 3);
}
"##,
  )
  .expect("write main.rs");

  let target_dir = project_dir.join("target");

  // Force clang+lld for the final link and explicitly pass the linker script
  // fragment that defines `__stackmaps_start` / `__stackmaps_end`.
  let mut rustflags = std::env::var("RUSTFLAGS").unwrap_or_default();
  if !rustflags.is_empty() {
    rustflags.push(' ');
  }
  // `runtime-native` enforces this (see `build.rs`); ensure the nested project
  // inherits it even though we override `RUSTFLAGS` below.
  rustflags.push_str("-C force-frame-pointers=yes ");
  rustflags.push_str(&format!(
    // Speed up the nested build: we only care about the link result (symbols +
    // section retention), not debug info.
    "-C debuginfo=0 -C linker=clang-18 -C link-arg=-fuse-ld=lld-18 -C link-arg=-Wl,-T,{} -C link-arg=-Wl,--gc-sections",
    stackmaps_ld.display()
  ));

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

  // Ensure the expected symbols were actually defined in the output (i.e. we
  // didn't accidentally pass this test via some fallback mechanism).
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

// Non-Linux targets: no-op. This test is about ELF + lld.
#[cfg(not(target_os = "linux"))]
#[test]
fn lld_linker_script_defines_stackmaps_range_symbols() {}
