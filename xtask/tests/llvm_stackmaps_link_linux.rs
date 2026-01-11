#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::tempdir;

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask crate should live under the workspace root")
    .to_path_buf()
}

fn run_ok(mut cmd: Command, what: &str) {
  let output = cmd.output().unwrap_or_else(|err| {
    panic!("failed to spawn {what}: {err}");
  });
  assert!(
    output.status.success(),
    "{what} failed.\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
    output.status,
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr),
  );
}

fn read_to_string(mut cmd: Command, what: &str) -> String {
  let output = cmd.output().unwrap_or_else(|err| {
    panic!("failed to spawn {what}: {err}");
  });
  assert!(
    output.status.success(),
    "{what} failed.\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
    output.status,
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr),
  );
  String::from_utf8_lossy(&output.stdout).to_string()
}

fn assert_tool_exists(tool: &str) {
  let status = Command::new("sh")
    .args(["-c", &format!("command -v {tool} >/dev/null 2>&1")])
    .status()
    .expect("spawn shell to check tool");
  assert!(
    status.success(),
    "missing required tool `{tool}` on Linux.\n\
Install LLVM 18 toolchain (clang-18/lld/llvm-objcopy) per vendor/ecma-rs/EXEC.plan.md."
  );
}

#[test]
fn linux_pie_stackmaps_link_succeeds_without_textrels() {
  assert_tool_exists("clang-18");
  assert_tool_exists("llvm-objcopy-18");
  assert_tool_exists("readelf");

  let repo_root = repo_root();
  let link_script = repo_root.join("vendor/ecma-rs/scripts/native_js_link_linux.sh");
  assert!(
    link_script.is_file(),
    "missing link driver script at {}",
    link_script.display()
  );

  let temp = tempdir().expect("tempdir");
  let dir = temp.path();

  // Emit `.llvm_stackmaps` via the `llvm.experimental.stackmap` intrinsic.
  //
  // The object file will contain absolute code address relocations in `.llvm_stackmaps`, which
  // normally breaks PIE linking with lld unless we apply our policy (make the section writable +
  // keep it via a linker script).
  let codegen_ll = dir.join("codegen.ll");
  fs::write(
    &codegen_ll,
    r#"; ModuleID = 'codegen'
target triple = "x86_64-pc-linux-gnu"

declare void @llvm.experimental.stackmap(i64, i32, ...)

define void @foo() {
entry:
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 1, i32 0)
  ret void
}
"#,
  )
  .expect("write codegen.ll");

  let codegen_o = dir.join("codegen.o");
  run_ok(
    Command::new("clang-18")
      .current_dir(dir)
      .args(["-c", "codegen.ll", "-o"])
      .arg(&codegen_o),
    "compile codegen.ll",
  );

  // Small C main that validates we can locate + read the stackmaps range.
  let main_c = dir.join("main.c");
  fs::write(
    &main_c,
    r#"#include <stddef.h>
#include <stdint.h>
#include <stdio.h>

extern const unsigned char __llvm_stackmaps_start[];
extern const unsigned char __llvm_stackmaps_end[];

extern void foo(void);

int main(void) {
  size_t size = (size_t)(__llvm_stackmaps_end - __llvm_stackmaps_start);
  if (size == 0) {
    fprintf(stderr, "empty .llvm_stackmaps (likely GC'd by the linker)\n");
    return 1;
  }

  unsigned version = (unsigned)__llvm_stackmaps_start[0];
  if (version != 3) {
    fprintf(stderr, "unexpected stackmap version: %u\n", version);
    return 2;
  }

  // Keep the stackmap-producing function reachable.
  foo();

  printf("stackmaps: version=%u size=%zu\n", version, size);
  return 0;
}
"#,
  )
  .expect("write main.c");

  let main_o = dir.join("main.o");
  run_ok(
    Command::new("clang-18")
      .current_dir(dir)
      .args(["-c", "main.c", "-o"])
      .arg(&main_o),
    "compile main.c",
  );

  // Link using the repo's policy wrapper (PIE, no textrel, keep stackmaps).
  let out = dir.join("app");
  run_ok(
    Command::new("bash")
      .arg(&link_script)
      .args(["--out"])
      .arg(&out)
      .arg("--")
      .arg(&main_o)
      .arg(&codegen_o),
    "link app via native_js_link_linux.sh",
  );
  assert!(out.is_file(), "expected output binary at {}", out.display());

  // Ensure we produced a PIE executable.
  let elf_header = read_to_string(Command::new("readelf").arg("-h").arg(&out), "readelf -h");
  assert!(
    elf_header.lines().any(|line| line.trim_start().starts_with("Type:") && line.contains("DYN")),
    "expected PIE (ET_DYN) output, got:\n{elf_header}",
  );

  // Ensure we did *not* enable TEXTREL (i.e., no `-z notext` workaround).
  let dynamic = read_to_string(Command::new("readelf").arg("-d").arg(&out), "readelf -d");
  assert!(
    !dynamic.contains("TEXTREL"),
    "expected no TEXTREL dynamic tag, got:\n{dynamic}",
  );

  // Run and sanity-check output.
  let run = Command::new(&out)
    .current_dir(dir)
    .output()
    .expect("run linked binary");
  assert!(
    run.status.success(),
    "binary failed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&run.stdout),
    String::from_utf8_lossy(&run.stderr),
  );

  let stdout = String::from_utf8_lossy(&run.stdout);
  assert!(
    stdout.contains("stackmaps: version=3"),
    "expected stackmap report, got stdout:\n{stdout}",
  );
}

