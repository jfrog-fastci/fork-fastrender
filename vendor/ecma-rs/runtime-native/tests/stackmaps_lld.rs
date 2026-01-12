#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::process::Command;

#[cfg(target_os = "linux")]
fn has_cmd(cmd: &str) -> bool {
  Command::new(cmd)
    .arg("--version")
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
}

#[cfg(target_os = "linux")]
fn find_cmd<'a>(candidates: &'a [&'a str]) -> Option<&'a str> {
  candidates.iter().copied().find(|c| has_cmd(c))
}

#[cfg(target_os = "linux")]
#[test]
fn lld_linker_script_defines_stackmaps_range_symbols() {
  let Some(nm) = find_cmd(&["llvm-nm-18", "llvm-nm"]) else {
    // Allow building in minimal environments; this test is specifically about
    // lld + linker-script behavior.
    return;
  };
  let Some(clang) = find_cmd(&["clang-18", "clang"]) else {
    // Allow building in minimal environments; this test is specifically about
    // lld + linker-script behavior.
    return;
  };
  let Some(lld_fuse) = (if has_cmd("ld.lld-18") {
    Some("lld-18")
  } else if has_cmd("ld.lld") {
    Some("lld")
  } else {
    None
  }) else {
    // Allow building in minimal environments; this test is specifically about
    // lld + linker-script behavior.
    return;
  };

  let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let stackmaps_ld = crate_dir.join("link/stackmaps.ld");

  let temp = tempfile::tempdir().expect("create tempdir");
  let project_dir = temp.path();

  std::fs::create_dir(project_dir.join("src")).expect("create src dir");

  std::fs::write(
    project_dir.join("Cargo.toml"),
    r#"[package]
name = "stackmaps_test_bin"
version = "0.0.0"
edition = "2021"
"#,
  )
  .expect("write Cargo.toml");

  // Define stackmaps bytes without creating any Rust-level references to them.
  //
  // In PIE/DSO mode, native-js rewrites `.llvm_stackmaps` to
  // `.data.rel.ro.llvm_stackmaps` so any relocations can be applied to writable
  // memory without requiring text relocations. `link/stackmaps.ld` therefore
  // `KEEP()`s `.data.rel.ro.llvm_stackmaps` inputs under `--gc-sections` (and may
  // append the payload into the standard `.data.rel.ro` output section).
  std::fs::write(
    project_dir.join("src/main.rs"),
    r##"use std::arch::global_asm;

global_asm!(
    r#"
    .section .data.rel.ro.llvm_stackmaps,"aw",@progbits
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

extern "C" {
    static __stackmaps_start: u8;
    static __stackmaps_end: u8;
}

fn main() {
    let start = unsafe { &__stackmaps_start as *const u8 };
    let end = unsafe { &__stackmaps_end as *const u8 };
    let len = unsafe { end.offset_from(start) as usize };
    assert!(len > 0, "expected non-empty stackmaps bytes");
    let bytes = unsafe { std::slice::from_raw_parts(start, len) };

    // Allow for alignment padding; search for our injected StackMap v3 header.
    const HEADER: [u8; 16] = [
        0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    assert!(
        bytes.windows(HEADER.len()).any(|w| w == HEADER),
        "expected stackmaps range to include the injected StackMap v3 header; len={len}"
    );
}
"##,
  )
  .expect("write main.rs");

  let workspace_root = crate_dir
    .parent()
    .expect("runtime-native should be nested under the vendor/ecma-rs workspace");
  let mut target_dir = std::env::var_os("CARGO_TARGET_DIR")
    .map(PathBuf::from)
    .unwrap_or_else(|| workspace_root.join("target"));
  if target_dir.is_relative() {
    target_dir = workspace_root.join(target_dir);
  }
  // Avoid deadlocking on Cargo's target-dir lock: use a separate target dir from the outer
  // `cargo test` process, but keep it stable so repeat runs can reuse build artifacts.
  let target_dir = target_dir.join("runtime_native_stackmaps_lld_test_target");

  // Force clang+lld for the final link and explicitly pass the linker script
  // fragment that defines `__stackmaps_start` / `__stackmaps_end`.
  let mut rustflags = std::env::var("RUSTFLAGS").unwrap_or_default();
  if !rustflags.is_empty() {
    rustflags.push(' ');
  }
  rustflags.push_str(&format!(
    // Speed up the nested build: we only care about the link result (symbols +
    // section retention), not debug info.
    "-C debuginfo=0 -C linker={clang} -C link-arg=-fuse-ld={lld_fuse} -C link-arg=-Wl,-T,{} -C link-arg=-Wl,--gc-sections",
    stackmaps_ld.display()
  ));

  let cargo_agent = workspace_root.join("scripts").join("cargo_agent.sh");
  let status = Command::new("bash")
    .arg(cargo_agent)
    .arg("build")
    .arg("--offline")
    .arg("--quiet")
    .arg("--manifest-path")
    .arg(project_dir.join("Cargo.toml"))
    .env("CARGO_TARGET_DIR", &target_dir)
    // The parent repo force-disables `build.rustc-wrapper` in its workspace config, but this
    // nested Cargo project lives in a tempdir and would otherwise inherit any globally configured
    // wrapper (often `sccache`), which can be flaky/unavailable in CI.
    .env("CARGO_BUILD_RUSTC_WRAPPER", "")
    .env("CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER", "")
    .env("RUSTFLAGS", rustflags)
    // Some environments configure a global `build.rustc-wrapper` (often `sccache`).
    // When this test spawns Cargo from a temp directory, it may not inherit this
    // repo's `.cargo/config.toml` override that disables the wrapper, and the
    // wrapper can flake (e.g. when the server is unavailable). Force-disable any
    // wrapper so this integration test is deterministic.
    .env("RUSTC_WRAPPER", "")
    .env("RUSTC_WORKSPACE_WRAPPER", "")
    .status()
    .expect("run nested build");
  assert!(status.success(), "nested build failed");

  let exe_path = target_dir.join("debug/stackmaps_test_bin");
  let status = Command::new(&exe_path)
    .status()
    .expect("run stackmaps_test_bin");
  assert!(status.success(), "stackmaps_test_bin failed");

  // Ensure the expected symbols were actually defined in the output (i.e. we
  // didn't accidentally pass this test via some fallback mechanism).
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
}

// Non-Linux targets: no-op. This test is about ELF + lld.
#[cfg(not(target_os = "linux"))]
#[test]
fn lld_linker_script_defines_stackmaps_range_symbols() {}
