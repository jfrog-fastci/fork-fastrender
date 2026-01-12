#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use anyhow::{bail, Context as _, Result};
use native_js::link::{LinkOpts, LinkerFlavor};
use std::fs;
use std::process::{Command, Stdio};

fn cmd_works(cmd: &str) -> bool {
  Command::new(cmd)
    .arg("--version")
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
}

fn find_clang() -> Option<&'static str> {
  for cand in ["clang-18", "clang"] {
    if cmd_works(cand) {
      return Some(cand);
    }
  }
  None
}

#[test]
fn links_runtime_native_and_runs_executable() -> Result<()> {
  let Some(clang) = find_clang() else {
    eprintln!("skipping: clang not found in PATH");
    return Ok(());
  };
  if !cmd_works("ld.lld-18") && !cmd_works("ld.lld") {
    eprintln!("skipping: lld not found in PATH (expected `ld.lld-18` or `ld.lld`)");
    return Ok(());
  }
  if !cmd_works("llvm-objcopy-18") && !cmd_works("llvm-objcopy") {
    eprintln!("skipping: llvm-objcopy not found in PATH (needed for lld stackmaps patching)");
    return Ok(());
  }

  let runtime_native_a = native_js::link::find_runtime_native_staticlib().context(
    "failed to locate runtime-native static library `libruntime_native.a` (set NATIVE_JS_RUNTIME_NATIVE_A=/path/to/libruntime_native.a)",
  )?;
  if !runtime_native_a.is_file() {
    bail!(
      "runtime-native static library not found at {} (set NATIVE_JS_RUNTIME_NATIVE_A=/path/to/libruntime_native.a)",
      runtime_native_a.display()
    );
  }

  let td = tempfile::tempdir().context("create tempdir")?;
  let ll = td.path().join("main.ll");
  let obj = td.path().join("main.o");
  let exe = td.path().join("rt_link_smoke");

  fs::write(
    &ll,
    r#"
; ModuleID = 'rt_link_smoke'
source_filename = "rt_link_smoke"

declare void @rt_thread_init(i32)
declare void @rt_thread_deinit()

define i32 @main() {
entry:
  call void @rt_thread_init(i32 0)
  call void @rt_thread_deinit()
  ret i32 0
}
"#,
  )
  .context("write main.ll")?;

  let out = Command::new(clang)
    .args(["-x", "ir", "-c"])
    .arg(&ll)
    .arg("-o")
    .arg(&obj)
    .output()
    .with_context(|| format!("compile {} with {clang}", ll.display()))?;
  if !out.status.success() {
    bail!(
      "{clang} failed to compile LLVM IR (status={status})\nstdout:\n{stdout}\nstderr:\n{stderr}",
      status = out.status,
      stdout = String::from_utf8_lossy(&out.stdout),
      stderr = String::from_utf8_lossy(&out.stderr),
    );
  }

  native_js::link::link_elf_executable_with_options_and_static_libs(
    &exe,
    &[obj],
    LinkOpts {
      // Match native-js defaults (lld + -no-pie on Linux).
      linker: LinkerFlavor::Lld,
      ..Default::default()
    },
    std::slice::from_ref(&runtime_native_a),
  )
  .context("link executable with native-js link helper")?;

  let status = Command::new(&exe)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .with_context(|| format!("run {}", exe.display()))?;
  if !status.success() {
    bail!("linked executable failed with status {status}");
  }

  Ok(())
}

