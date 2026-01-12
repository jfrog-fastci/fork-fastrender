use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use runtime_native::stackmap_loader;

fn cmd_exists(cmd: &str) -> bool {
  Command::new(cmd)
    .arg("--version")
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .map(|s| s.success())
    .unwrap_or(false)
}

fn find_tool(candidates: &[&'static str]) -> Option<&'static str> {
  for &cand in candidates {
    if cmd_exists(cand) {
      return Some(cand);
    }
  }
  None
}

fn run(cmd: &str, args: &[&str]) {
  let status = Command::new(cmd)
    .args(args)
    .status()
    .unwrap_or_else(|e| panic!("failed to run {cmd}: {e}"));
  assert!(
    status.success(),
    "{cmd} {:?} failed with status {status}",
    args
  );
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

fn write(path: &Path, contents: &str) {
  fs::write(path, contents).unwrap_or_else(|e| panic!("failed to write {path:?}: {e}"));
}

#[test]
fn pie_stackmaps_have_no_dt_textrel_and_loader_finds_section() {
  if !cfg!(target_os = "linux") {
    return;
  }

  for tool in ["gcc", "readelf", "bash"] {
    if !cmd_exists(tool) {
      eprintln!("skipping: missing tool {tool}");
      return;
    }
  }

  let Some(llc) = find_tool(&["llc-18", "llc"]) else {
    eprintln!("skipping: llc not found in PATH (need llc-18 or llc)");
    return;
  };
  // `rename_llvm_stackmaps_section.sh` uses llvm-readobj/llvm-objcopy, so ensure they're available
  // before spawning the helper.
  let Some(readobj) = find_tool(&["llvm-readobj-18", "llvm-readobj"]) else {
    eprintln!("skipping: llvm-readobj not found in PATH (need llvm-readobj-18 or llvm-readobj)");
    return;
  };
  let Some(_objcopy) = find_tool(&["llvm-objcopy-18", "llvm-objcopy"]) else {
    eprintln!("skipping: llvm-objcopy not found in PATH (need llvm-objcopy-18 or llvm-objcopy)");
    return;
  };

  let tmp = tempfile::tempdir().expect("tempdir");
  let dir = tmp.path();

  let foo1_ll = dir.join("foo1.ll");
  let foo2_ll = dir.join("foo2.ll");
  let foo1_o = dir.join("foo1.o");
  let foo2_o = dir.join("foo2.o");
  let main_c = dir.join("main.c");
  let main_o = dir.join("main.o");
  let ld_script = dir.join("stackmaps.ld");
  let exe = dir.join("a_pie");

  write(
    &foo1_ll,
    r#"
source_filename = "foo1"
declare void @llvm.experimental.stackmap(i64, i32, ...)
define void @foo1(ptr %p) {
entry:
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 1, i32 0, ptr %p)
  ret void
}
"#,
  );
  write(
    &foo2_ll,
    r#"
source_filename = "foo2"
declare void @llvm.experimental.stackmap(i64, i32, ...)
define void @foo2(ptr %p) {
entry:
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 2, i32 0, ptr %p)
  ret void
}
"#,
  );

  run(
    llc,
    &[
      "-filetype=obj",
      "-relocation-model=pic",
      foo1_ll.to_str().unwrap(),
      "-o",
      foo1_o.to_str().unwrap(),
    ],
  );
  run(
    llc,
    &[
      "-filetype=obj",
      "-relocation-model=pic",
      foo2_ll.to_str().unwrap(),
      "-o",
      foo2_o.to_str().unwrap(),
    ],
  );

  // Sanity check: object contains the legacy section before rename.
  let foo1_sections = output(readobj, &["--sections", foo1_o.to_str().unwrap()]);
  assert!(
    foo1_sections.contains(".llvm_stackmaps"),
    "expected foo1.o to contain .llvm_stackmaps before rename; got:\n{foo1_sections}"
  );

  let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let repo_root = manifest_dir.parent().expect("runtime-native/..");
  let rename_script = repo_root.join("scripts/rename_llvm_stackmaps_section.sh");
  assert!(
    rename_script.exists(),
    "missing helper script at {rename_script:?}"
  );

  run(
    "bash",
    &[
      rename_script.to_str().unwrap(),
      foo1_o.to_str().unwrap(),
      foo2_o.to_str().unwrap(),
    ],
  );

  // Force the output stackmap section to stay named `.data.rel.ro.llvm_stackmaps` so the runtime
  // loader can find it by name (and so the test doesn't depend on default linker-script folding).
  write(
    &ld_script,
    r#"
SECTIONS {
  .data.rel.ro.llvm_stackmaps : { KEEP(*(.data.rel.ro.llvm_stackmaps)) }
}
INSERT BEFORE .data.rel.ro;
"#,
  );

  write(
    &main_c,
    r#"
#include <stdint.h>
void foo1(void*);
void foo2(void*);
int main() {
  foo1((void*)0);
  foo2((void*)0);
  return 0;
}
"#,
  );

  run(
    "gcc",
    &[
      "-fPIE",
      "-c",
      main_c.to_str().unwrap(),
      "-o",
      main_o.to_str().unwrap(),
    ],
  );

  let ld_arg = format!("-Wl,-T,{}", ld_script.to_str().unwrap());
  run(
    "gcc",
    &[
      "-pie",
      // Regression guard: section GC drops unreferenced stackmaps unless the
      // linker script explicitly `KEEP()`s the section.
      "-Wl,--gc-sections",
      ld_arg.as_str(),
      main_o.to_str().unwrap(),
      foo1_o.to_str().unwrap(),
      foo2_o.to_str().unwrap(),
      "-o",
      exe.to_str().unwrap(),
    ],
  );

  let dynamic = output("readelf", &["-d", exe.to_str().unwrap()]);
  assert!(
    !dynamic.contains("TEXTREL"),
    "expected no DT_TEXTREL; got:\n{dynamic}"
  );

  let elf = fs::read(&exe).expect("read exe");
  let section = stackmap_loader::find_stackmap_section(&elf)
    .expect("load stackmaps")
    .expect("stackmaps section missing");
  assert_eq!(
    section.source,
    stackmap_loader::StackMapSectionSource::SectionName(".data.rel.ro.llvm_stackmaps"),
    "expected stackmaps to be discovered by section lookup"
  );

  let blobs =
    stackmap_loader::parse_stackmap_blobs(section.bytes).expect("parse stackmaps section");
  let mut ids: Vec<u64> = blobs.into_iter().flat_map(|b| b.record_ids).collect();
  ids.sort_unstable();
  ids.dedup();
  assert!(
    ids.contains(&1) && ids.contains(&2),
    "expected record IDs to contain 1 and 2; got {ids:?}"
  );
}
