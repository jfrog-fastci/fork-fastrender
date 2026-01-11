#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn cmd_exists(cmd: &str) -> bool {
  Command::new(cmd)
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
}

fn output(cmd: &str, args: &[&str]) -> String {
  let out = Command::new(cmd)
    .args(args)
    .output()
    .unwrap_or_else(|e| panic!("failed to run {cmd}: {e}"));
  assert!(
    out.status.success(),
    "{cmd} {:?} failed with status {}",
    args,
    out.status
  );
  String::from_utf8_lossy(&out.stdout).into_owned()
}

fn run(cmd: &mut Command) {
  let out = cmd
    .output()
    .unwrap_or_else(|e| panic!("failed to run {cmd:?}: {e}"));
  assert!(
    out.status.success(),
    "command failed (status={}):\n  cmd={cmd:?}\n  stdout:\n{}\n  stderr:\n{}\n",
    out.status,
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr),
  );
}

fn write(path: &Path, contents: &str) {
  fs::write(path, contents).unwrap_or_else(|e| panic!("failed to write {path:?}: {e}"));
}

fn find_clang() -> Option<&'static str> {
  for cand in ["clang-18", "clang"] {
    if cmd_exists(cand) {
      return Some(cand);
    }
  }
  None
}

#[test]
fn native_link_ld_non_pie_does_not_produce_rwx_when_stackmaps_are_writable() {
  for tool in ["bash", "readelf"] {
    if !cmd_exists(tool) {
      eprintln!("skipping: missing tool {tool}");
      return;
    }
  }
  let Some(clang) = find_clang() else {
    eprintln!("skipping: clang not found (need clang-18 or clang)");
    return;
  };

  // This is a GNU ld-specific regression. If `clang`'s system linker isn't GNU ld, skip.
  let ld_version = output("ld", &["--version"]);
  if !ld_version.contains("GNU ld") {
    eprintln!("skipping: system ld is not GNU ld (got: {ld_version:?})");
    return;
  }

  let td = tempfile::tempdir().expect("tempdir");
  let dir = td.path();

  // Build an object that already contains a writable `.data.rel.ro.llvm_stackmaps` section. If a
  // non-PIE linker script inserts this section immediately after `.text`, GNU ld can merge it into
  // the text PT_LOAD and produce an RWX segment.
  let sm_s = dir.join("sm.S");
  write(
    &sm_s,
    r#"
  .text
  .globl sm_fn
  .type sm_fn, @function
sm_fn:
  ret

  .section .data.rel.ro.llvm_stackmaps,"aw",@progbits
  .byte 1,2,3,4

  .section .note.GNU-stack,"",@progbits
"#,
  );
  let sm_o = dir.join("sm.o");
  run(Command::new(clang).args(["-c", "-o"]).arg(&sm_o).arg(&sm_s));

  let main_c = dir.join("main.c");
  write(
    &main_c,
    r#"
void sm_fn(void);
int main() {
  sm_fn();
  return 0;
}
"#,
  );
  let main_o = dir.join("main.o");
  run(
    Command::new(clang)
      .args(["-c", "-o"])
      .arg(&main_o)
      .arg(&main_c),
  );

  let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let repo_root = manifest_dir.parent().expect("runtime-native/..");
  let native_link = repo_root.join("scripts/native_link.sh");
  assert!(native_link.exists(), "missing wrapper at {native_link:?}");

  let exe = dir.join("a.out");
  let mut cmd = Command::new("bash");
  cmd.arg(&native_link)
    .arg("-o")
    .arg(&exe)
    .arg(&main_o)
    .arg(&sm_o)
    .env("ECMA_RS_NATIVE_LINKER", "ld")
    .env("ECMA_RS_NATIVE_PIE", "0")
    // Use `--gc-sections` to ensure the wrapper's injected linker fragment actually keeps the
    // otherwise-unreferenced stackmaps section.
    .env("ECMA_RS_NATIVE_GC_SECTIONS", "1");
  run(&mut cmd);

  let segments = output("readelf", &["-W", "-l", exe.to_str().unwrap()]);
  for line in segments.lines() {
    let trimmed = line.trim_start();
    if !trimmed.starts_with("LOAD") {
      continue;
    }
    // `readelf -l` output contains a combined `RWE` flag column; reject any LOAD segment with W+E.
    assert!(
      !trimmed.contains("RWE"),
      "found RWX LOAD segment in native_link.sh output:\n{trimmed}\n\nfull readelf -l output:\n{segments}"
    );
  }
}

