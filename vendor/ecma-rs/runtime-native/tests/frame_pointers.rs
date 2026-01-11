#![cfg(target_arch = "x86_64")]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn cmd_output(mut cmd: Command) -> String {
  let output = cmd.output().unwrap_or_else(|e| {
    panic!("failed to spawn {:?}: {e}", cmd);
  });
  if !output.status.success() {
    panic!(
      "command failed: {:?}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
      cmd,
      output.status,
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr),
    );
  }
  String::from_utf8_lossy(&output.stdout).into_owned()
}

fn find_on_path(candidates: &[&str]) -> Option<PathBuf> {
  for &name in candidates {
    let ok = Command::new(name)
      .arg("--version")
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::null())
      .status()
      .is_ok_and(|s| s.success());
    if ok {
      return Some(PathBuf::from(name));
    }
  }
  None
}

fn disassemble(path: &Path) -> String {
  let objdump = find_on_path(&["llvm-objdump-18", "llvm-objdump", "objdump"]).unwrap_or_else(|| {
    panic!(
      "no objdump found in PATH (need llvm-objdump or binutils objdump) to disassemble {path:?}"
    )
  });
  let mut cmd = Command::new(objdump);
  cmd.arg("-d").arg(path);
  cmd_output(cmd)
}

fn disassemble_rlib(rlib_path: &Path, extract_dir: &Path) -> String {
  fs::create_dir_all(extract_dir).unwrap();

  // Extract archive members so we can disassemble the actual `.o` file(s).
  // `llvm-objdump` can be picky about `.rlib` archives (it may only print the
  // `lib.rmeta` member), so disassembling the extracted objects is more
  // portable.
  let mut cmd = Command::new("ar");
  cmd.current_dir(extract_dir).arg("x").arg(rlib_path);
  cmd_output(cmd);

  let mut out = String::new();
  for entry in fs::read_dir(extract_dir).unwrap() {
    let entry = entry.unwrap();
    let path = entry.path();
    if path.extension().and_then(|s| s.to_str()) != Some("o") {
      continue;
    }
    out.push_str(&disassemble(&path));
    out.push('\n');
  }
  out
}

fn assert_has_fp_prologue(disasm: &str, symbol: &str) {
  let marker = format!("<{symbol}>:");
  let start = disasm
    .find(&marker)
    .unwrap_or_else(|| panic!("symbol {symbol:?} not found in disassembly"));
  let excerpt: Vec<&str> = disasm[start..].lines().take(30).collect();

  let mut saw_push_rbp = false;
  for line in &excerpt {
    let l = line.to_ascii_lowercase();
    if !saw_push_rbp {
      if l.contains("push") && l.contains("rbp") {
        saw_push_rbp = true;
      }
      continue;
    }

    if l.contains("mov") && l.contains("rsp") && l.contains("rbp") {
      return;
    }
  }

  panic!(
    "expected x86_64 frame-pointer prologue for {symbol:?} (push rbp; mov rbp, rsp)\nexcerpt:\n{}",
    excerpt.join("\n")
  );
}

#[test]
#[cfg(target_arch = "x86_64")]
fn runtime_native_release_has_frame_pointers() {
  let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let workspace_root = crate_dir
    .parent()
    .expect("runtime-native should live under the vendor/ecma-rs workspace");

  let tmp = tempfile::tempdir().unwrap();
  let project_dir = tmp.path().join("project");
  fs::create_dir_all(project_dir.join("src")).unwrap();

  fs::write(
    project_dir.join("Cargo.toml"),
    format!(
      r#"[package]
name = "fp_test_bin"
version = "0.0.0"
edition = "2021"

[dependencies]
runtime-native = {{ path = "{}", features = ["fp_regression"] }}
"#,
      crate_dir.display()
    ),
  )
  .expect("write Cargo.toml");
  fs::write(
    project_dir.join("src/main.rs"),
    r#"fn main() {
  // Force a reference so the dependency is not pruned by any link-time tooling.
  let _ = runtime_native::rt_fp_test_entry(123);
}
"#,
  )
  .expect("write main.rs");

  let mut target_dir = env::var_os("CARGO_TARGET_DIR")
    .map(PathBuf::from)
    .unwrap_or_else(|| workspace_root.join("target"));
  if target_dir.is_relative() {
    target_dir = workspace_root.join(target_dir);
  }
  // Avoid deadlocking on Cargo's target-dir lock: use a separate target dir from the outer
  // `cargo test` process, but keep it stable so repeat runs can reuse release artifacts.
  let target_dir = target_dir.join("runtime_native_fp_regression_test_target");

  let mut rustflags = env::var("RUSTFLAGS").unwrap_or_default();
  if !rustflags.is_empty() {
    rustflags.push(' ');
  }
  rustflags.push_str("-C force-frame-pointers=yes");

  // Build a tiny crate that depends on `runtime-native` with `fp_regression` enabled.
  //
  // IMPORTANT: build `runtime-native` as a dependency so Cargo only produces an `rlib` (building the
  // `runtime-native` package directly would also build its `staticlib` + `cdylib`, which is much
  // slower and unnecessary for this regression test).
  let cargo_agent = workspace_root.join("scripts").join("cargo_agent.sh");
  let manifest_path = project_dir.join("Cargo.toml");
  let mut cmd = Command::new("bash");
  cmd
    .arg(cargo_agent)
    .current_dir(workspace_root)
    .env("CARGO_TARGET_DIR", &target_dir)
    // Ensure the `.rlib` contains disassemblable object files (not LLVM bitcode).
    .env("CARGO_PROFILE_RELEASE_LTO", "false")
    .env("CARGO_PROFILE_RELEASE_STRIP", "none")
    .env("RUSTFLAGS", rustflags)
    .arg("build")
    .arg("--offline")
    .arg("--manifest-path")
    .arg(&manifest_path)
    .arg("--release")
    .arg("--quiet");
  cmd_output(cmd);

  let deps_dir = target_dir.join("release").join("deps");
  let mut rlib_path = None;
  for entry in fs::read_dir(&deps_dir).unwrap() {
    let entry = entry.unwrap();
    let path = entry.path();
    let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
      continue;
    };
    if file_name == "libruntime_native.rlib"
      || (file_name.starts_with("libruntime_native-") && file_name.ends_with(".rlib"))
    {
      rlib_path = Some(path);
      break;
    }
  }
  let rlib_path = rlib_path.unwrap_or_else(|| panic!("unable to find runtime-native rlib in {deps_dir:?}"));

  let disasm = disassemble_rlib(&rlib_path, &tmp.path().join("rlib_extract"));
  assert_has_fp_prologue(&disasm, "rt_fp_test_entry");
  assert_has_fp_prologue(&disasm, "rt_fp_test_mid");
  assert_has_fp_prologue(&disasm, "rt_fp_test_leaf");
}

#[test]
#[cfg(target_arch = "x86_64")]
fn llc_generated_object_has_frame_pointers() {
  let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("runtime-native should live under the vendor/ecma-rs workspace");

  let tmp = tempfile::tempdir().unwrap();
  let ll_path = tmp.path().join("managed_fp_test.ll");
  let obj_path = tmp.path().join("managed_fp_test.o");

  // Minimal "rewritten statepoint" style IR (gc.statepoint + gc.relocate).
  // This is produced by `opt -passes=rewrite-statepoints-for-gc` on a tiny
  // example and kept inline so the test stays self-contained.
  //
  // We only assert on the function prologue — the FP ABI requirement is
  // independent of the GC lowering details.
  fs::write(
    &ll_path,
    r#"; ModuleID = 'managed_fp_test'
source_filename = "managed_fp_test"
target datalayout = "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-f80:128-n8:16:32:64-S128"
target triple = "x86_64-unknown-linux-gnu"

declare ptr addrspace(1) @allocate(i64)
declare void @consume(ptr addrspace(1), ptr addrspace(1))

define ptr addrspace(1) @managed_fp_test(ptr addrspace(1) %a, ptr addrspace(1) %b) gc "coreclr" {
entry:
  %statepoint_token = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(i64 2882400000, i32 0, ptr elementtype(ptr addrspace(1) (i64)) @allocate, i32 1, i32 0, i64 16, i32 0, i32 0) [ "gc-live"(ptr addrspace(1) %a, ptr addrspace(1) %b) ]
  %pair1 = call ptr addrspace(1) @llvm.experimental.gc.result.p1(token %statepoint_token)
  %a.relocated = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %statepoint_token, i32 0, i32 0) ; (%a, %a)
  %b.relocated = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %statepoint_token, i32 1, i32 1) ; (%b, %b)
  %statepoint_token2 = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(i64 2882400000, i32 0, ptr elementtype(void (ptr addrspace(1), ptr addrspace(1))) @consume, i32 2, i32 0, ptr addrspace(1) %a.relocated, ptr addrspace(1) %b.relocated, i32 0, i32 0) [ "gc-live"(ptr addrspace(1) %pair1, ptr addrspace(1) %a.relocated, ptr addrspace(1) %b.relocated) ]
  %pair.relocated = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %statepoint_token2, i32 0, i32 0) ; (%pair1, %pair1)
  %a.relocated3 = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %statepoint_token2, i32 1, i32 1) ; (%a.relocated, %a.relocated)
  %b.relocated4 = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %statepoint_token2, i32 2, i32 2) ; (%b.relocated, %b.relocated)
  ret ptr addrspace(1) %pair.relocated
}

; Leaf function: without `--frame-pointer=all`, LLVM is free to omit the frame pointer chain.
define i64 @managed_fp_leaf(i64 %x) {
entry:
  %y = add i64 %x, 1
  ret i64 %y
}

declare token @llvm.experimental.gc.statepoint.p0(i64 immarg, i32 immarg, ptr, i32 immarg, i32 immarg, ...)
declare ptr addrspace(1) @llvm.experimental.gc.result.p1(token) #0
declare ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token, i32 immarg, i32 immarg) #0

attributes #0 = { nocallback nofree nosync nounwind willreturn memory(none) }
"#,
  )
  .unwrap();

  let llc_wrapper = workspace_root.join("scripts").join("llc_fp.sh");
  let mut cmd = Command::new("bash");
  cmd
    .arg(llc_wrapper)
    .args(["-O3", "-filetype=obj"])
    .arg("-o")
    .arg(&obj_path)
    .arg(&ll_path);
  cmd_output(cmd);

  let disasm = disassemble(&obj_path);
  assert_has_fp_prologue(&disasm, "managed_fp_test");
  assert_has_fp_prologue(&disasm, "managed_fp_leaf");
}
