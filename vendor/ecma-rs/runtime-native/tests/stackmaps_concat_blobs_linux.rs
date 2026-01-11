#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use object::{Object, ObjectSection};
use runtime_native::stackmaps::{parse_all_stackmaps, StackMap, StackMaps};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn command_works(cmd: &str) -> bool {
  Command::new(cmd)
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok()
}

fn find_llc() -> Option<&'static str> {
  for cand in ["llc-18", "llc"] {
    if command_works(cand) {
      return Some(cand);
    }
  }
  None
}

fn find_clang() -> Option<&'static str> {
  for cand in ["clang-18", "clang"] {
    if command_works(cand) {
      return Some(cand);
    }
  }
  None
}

fn run(cmd: &mut Command) {
  let status = cmd.status().unwrap_or_else(|err| panic!("failed to run {cmd:?}: {err}"));
  assert!(status.success(), "command failed: {cmd:?}");
}

fn emit_obj(llc: &str, ir_path: &Path, obj_path: &Path) {
  run(
    Command::new(llc)
      .arg("-filetype=obj")
      .arg(ir_path)
      .arg("-o")
      .arg(obj_path),
  );
  assert!(obj_path.exists(), "missing output object {}", obj_path.display());
}

fn link_exe(clang: &str, out: &Path, objs: &[PathBuf]) {
  let mut cmd = Command::new(clang);
  cmd.arg("-no-pie");
  for obj in objs {
    cmd.arg(obj);
  }
  cmd.arg("-o").arg(out);
  run(&mut cmd);
  assert!(out.exists(), "missing output executable {}", out.display());
}

fn llvm_stackmaps_section(exe: &Path) -> Vec<u8> {
  let bytes = fs::read(exe).expect("read linked executable");
  let file = object::File::parse(&*bytes).expect("parse linked executable");
  let section = file
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section (linker GC?)");
  section
    .data()
    .expect("read .llvm_stackmaps section bytes")
    .to_vec()
}

fn callsites_for_stackmap(sm: &StackMap) -> Vec<(u64, u64)> {
  let mut out = Vec::new();
  let mut record_index: usize = 0;
  for f in &sm.functions {
    let rc = usize::try_from(f.record_count).expect("record_count fits usize");
    for _ in 0..rc {
      let rec = &sm.records[record_index];
      let pc = f
        .address
        .checked_add(rec.instruction_offset as u64)
        .expect("pc overflow");
      out.push((pc, rec.patchpoint_id));
      record_index += 1;
    }
  }
  out
}

const MOD_A: &str = r#"
; ModuleID = 'stackmaps_concat_a'
target triple = "x86_64-unknown-linux-gnu"

declare void @llvm.experimental.stackmap(i64, i32, ...)
declare void @foo()

define void @a_func() {
entry:
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 111, i32 0)
  ret void
}

define i32 @main() {
entry:
  call void @a_func()
  call void @foo()
  ret i32 0
}
"#;

const MOD_B: &str = r#"
; ModuleID = 'stackmaps_concat_b'
target triple = "x86_64-unknown-linux-gnu"

declare void @llvm.experimental.stackmap(i64, i32, ...)

define void @foo() {
entry:
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 222, i32 0)
  ret void
}
"#;

#[test]
fn parses_linker_concatenated_stackmap_blobs_and_indexes_all_callsites() {
  let Some(llc) = find_llc() else {
    eprintln!("skipping: llc-18 not found in PATH");
    return;
  };
  let Some(clang) = find_clang() else {
    eprintln!("skipping: clang-18 not found in PATH");
    return;
  };

  let td = tempfile::tempdir().expect("create tempdir");

  let a_ll = td.path().join("a.ll");
  let b_ll = td.path().join("b.ll");
  fs::write(&a_ll, MOD_A).expect("write a.ll");
  fs::write(&b_ll, MOD_B).expect("write b.ll");

  let a_o = td.path().join("a.o");
  let b_o = td.path().join("b.o");
  emit_obj(llc, &a_ll, &a_o);
  emit_obj(llc, &b_ll, &b_o);

  let exe = td.path().join("a.out");
  link_exe(clang, &exe, &[a_o, b_o]);

  let stackmaps_bytes = llvm_stackmaps_section(&exe);
  assert!(!stackmaps_bytes.is_empty(), "expected non-empty .llvm_stackmaps");

  let blobs = parse_all_stackmaps(&stackmaps_bytes).expect("parse concatenated stackmap blobs");
  assert_eq!(
    blobs.len(),
    2,
    "expected 2 concatenated StackMap blobs in final .llvm_stackmaps section"
  );

  let mut expected_callsites: Vec<(u64, u64)> = Vec::new();
  for sm in &blobs {
    expected_callsites.extend(callsites_for_stackmap(sm));
  }

  // Ensure both compilation units contributed at least one callsite record.
  let ids: Vec<u64> = expected_callsites.iter().map(|(_, id)| *id).collect();
  assert!(ids.contains(&111), "missing patchpoint_id=111 in {ids:?}");
  assert!(ids.contains(&222), "missing patchpoint_id=222 in {ids:?}");

  let index = StackMaps::parse(&stackmaps_bytes).expect("parse + index concatenated stackmaps");

  for (pc, patchpoint_id) in expected_callsites {
    let callsite = index
      .lookup(pc)
      .unwrap_or_else(|| panic!("missing indexed callsite for pc=0x{pc:x}"));
    assert_eq!(
      callsite.record.patchpoint_id, patchpoint_id,
      "wrong record for pc=0x{pc:x}"
    );
  }
}

