use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn find_c_compiler() -> Option<String> {
  // Prefer $CC when set (common in CI / cross toolchains).
  if let Ok(cc) = std::env::var("CC") {
    if !cc.trim().is_empty() {
      return Some(cc);
    }
  }

  // Ubuntu images usually provide `cc`. Fall back to clang/gcc when needed.
  for candidate in ["cc", "clang", "gcc"] {
    if Command::new(candidate)
      .arg("--version")
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::null())
      .status()
      .is_ok()
    {
      return Some(candidate.to_string());
    }
  }

  None
}

fn workspace_root() -> PathBuf {
  // runtime-native/ is a workspace member; workspace root is its parent.
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("runtime-native should live at <workspace>/runtime-native")
    .to_path_buf()
}

fn cargo_bin() -> String {
  std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

#[test]
fn c_can_link_and_call_runtime_native() {
  if !cfg!(unix) {
    eprintln!("skipping: C link smoke test only runs on unix-like targets");
    return;
  }

  let Some(cc) = find_c_compiler() else {
    eprintln!("skipping: no C compiler (`cc`/`clang`/`gcc`) available");
    return;
  };

  let tmp = tempfile::tempdir().expect("create temp dir");
  let build_target_dir = tmp.path().join("cargo-target");

  // Avoid deadlocking on Cargo's target-dir lock: the outer `cargo test` process holds a lock on
  // its own target directory for the duration of test execution. We build the staticlib into a
  // separate temp target dir instead.
  let build = Command::new(cargo_bin())
    .current_dir(workspace_root())
    .env("CARGO_TARGET_DIR", &build_target_dir)
    .arg("build")
    .arg("-p")
    .arg("runtime-native")
    .arg("--release")
    .status()
    .expect("build runtime-native staticlib");

  assert!(build.success(), "cargo build failed: {build:?}");

  let staticlib = build_target_dir
    .join("release")
    .join("libruntime_native.a");
  assert!(
    staticlib.exists(),
    "missing staticlib at {} after build",
    staticlib.display()
  );

  let c_path = tmp.path().join("smoke.c");
  let bin_path = tmp.path().join("smoke");

  fs::write(
    &c_path,
    r#"
#include "runtime_native.h"

int main(void) {
  rt_gc_safepoint();
  return 0;
}
"#,
  )
  .expect("write smoke.c");

  let include_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("include");

  let compile = Command::new(cc)
    .arg("-std=c99")
    .arg("-I")
    .arg(&include_dir)
    .arg(&c_path)
    .arg(&staticlib)
    .arg("-o")
    .arg(&bin_path)
    .status()
    .expect("compile + link smoke.c");

  assert!(
    compile.success(),
    "C compile/link failed with status: {compile:?}"
  );

  let run = Command::new(&bin_path)
    .status()
    .expect("run linked C binary");

  assert!(run.success(), "C binary exited non-zero: {run:?}");
}
