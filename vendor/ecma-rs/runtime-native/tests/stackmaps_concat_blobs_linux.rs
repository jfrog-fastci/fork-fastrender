#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use object::{Object, ObjectSection};
use runtime_native::stackmaps::{parse_all_stackmaps, StackMap, StackMaps};
use runtime_native::test_util::TestRuntimeGuard;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn command_works(cmd: &str) -> bool {
  Command::new(cmd)
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
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

fn lld_flag() -> Option<&'static str> {
  if command_works("ld.lld-18") {
    Some("-fuse-ld=lld-18")
  } else if command_works("ld.lld") {
    Some("-fuse-ld=lld")
  } else {
    None
  }
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
  let mk = |lld_flag: Option<&str>| {
    let mut cmd = Command::new(clang);
    if let Some(flag) = lld_flag {
      cmd.arg(flag);
    }
    cmd.arg("-no-pie");
    for obj in objs {
      cmd.arg(obj);
    }
    cmd.arg("-o").arg(out);
    cmd
  };

  // Prefer lld to match production builds (`clang -fuse-ld=lld`). If lld isn't
  // installed, fall back to the system default linker so the test still runs.
  //
  // Prefer version-suffixed lld if present.
  let lld_flag = lld_flag();
  if let Some(flag) = lld_flag {
    let mut cmd = mk(Some(flag));
    let status = cmd
      .status()
      .unwrap_or_else(|err| panic!("failed to run {cmd:?}: {err}"));
    if !status.success() {
      eprintln!("warning: {cmd:?} failed; retrying without {flag}");
      let mut cmd = mk(None);
      run(&mut cmd);
    }
  } else {
    let mut cmd = mk(None);
    run(&mut cmd);
  }
  assert!(out.exists(), "missing output executable {}", out.display());
}

fn llvm_stackmaps_section(exe: &Path) -> Vec<u8> {
  let bytes = fs::read(exe).expect("read linked executable");
  let file = object::File::parse(&*bytes).expect("parse linked executable");
  let section = file
    .section_by_name(".data.rel.ro.llvm_stackmaps")
    .or_else(|| file.section_by_name(".llvm_stackmaps"))
    .expect("missing stackmaps section (linker GC?)");
  section
    .data()
    .expect("read stackmaps section bytes")
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
    eprintln!("skipping: llc not found in PATH (need llc-18 or llc)");
    return;
  };
  let Some(clang) = find_clang() else {
    eprintln!("skipping: clang not found in PATH (need clang-18 or clang)");
    return;
  };

  let _rt = TestRuntimeGuard::new();

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
  assert!(!stackmaps_bytes.is_empty(), "expected non-empty stackmaps section");

  let blobs = parse_all_stackmaps(&stackmaps_bytes).expect("parse concatenated stackmap blobs");
  assert_eq!(
    blobs.len(),
    2,
    "expected 2 concatenated StackMap blobs in final stackmaps section"
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
