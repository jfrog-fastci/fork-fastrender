#![cfg(all(target_arch = "x86_64", target_os = "linux"))]

use std::fs;
use std::process::Command;

use runtime_native::stackmaps::{Location, StackMaps};
use tempfile::tempdir;

fn have_tool(name: &str) -> bool {
  Command::new(name).arg("--version").output().is_ok()
}

fn run(cmd: &mut Command) {
  let status = cmd.status().expect("failed to spawn command");
  assert!(status.success(), "command failed: {cmd:?}");
}

#[test]
fn parses_llvm18_statepoint_stackmaps_and_finds_reloc_pair_offsets() {
  if !(have_tool("llvm-as-18") && have_tool("opt-18") && have_tool("llc-18") && have_tool("llvm-objcopy-18")) {
    eprintln!("skipping: required LLVM 18 tools not found in PATH");
    return;
  }

  let dir = tempdir().unwrap();
  let ll = dir.path().join("test.ll");
  let bc = dir.path().join("test.bc");
  let opt_bc = dir.path().join("test.opt.bc");
  let obj = dir.path().join("test.o");
  let stackmaps_bin = dir.path().join("stackmaps.bin");

  // Minimal statepoint+relocate using the LLVM 18 operand bundle form:
  // - Two trailing i32 zeros: numTransitionArgs=0, numDeoptArgs=0
  // - GC live pointers provided via the "gc-live" operand bundle.
  fs::write(
    &ll,
    r#"
      source_filename = "stackmaps_test"
      target datalayout = "e-m:e-i64:64-f80:128-n8:16:32:64-S128"
      target triple = "x86_64-pc-linux-gnu"

      declare void @callee() "gc-leaf-function"
      declare token @llvm.experimental.gc.statepoint.p0(i64, i32, ptr, i32, i32, ...)
      declare ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token, i32, i32)

      define void @foo(ptr addrspace(1) %base) gc "coreclr" {
      entry:
        %derived = getelementptr i8, ptr addrspace(1) %base, i64 16
        %tok = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
          i64 0,
          i32 0,
          ptr elementtype(void ()) @callee,
          i32 0,
          i32 0,
          i32 0,
          i32 0) [ "gc-live"(ptr addrspace(1) %base, ptr addrspace(1) %derived) ]
        %rel = call ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %tok, i32 0, i32 1)
        ret void
      }
    "#,
  )
  .unwrap();

  run(Command::new("llvm-as-18").arg(&ll).arg("-o").arg(&bc));
  run(
    Command::new("opt-18")
      .arg("-passes=rewrite-statepoints-for-gc")
      .arg(&bc)
      .arg("-o")
      .arg(&opt_bc),
  );
  run(
    Command::new("llc-18")
      .arg("-filetype=obj")
      .arg("-O0")
      .arg("-frame-pointer=all")
      .arg(&opt_bc)
      .arg("-o")
      .arg(&obj),
  );

  run(
    Command::new("llvm-objcopy-18")
      .arg(format!("--dump-section"))
      .arg(format!(".llvm_stackmaps={}", stackmaps_bin.display()))
      .arg(&obj),
  );

  let stackmaps = fs::read(&stackmaps_bin).unwrap();
  let index = StackMaps::parse(&stackmaps).unwrap();

  let callsites = index.callsites();
  assert_eq!(callsites.len(), 1, "expected one callsite record");
  let pc = callsites[0].pc;
  let callsite = index.lookup(pc).expect("record lookup by pc failed");
  let record = callsite.record;

  assert_eq!(record.locations.len(), 5, "expected 3 const + 2 slots");
  assert!(matches!(record.locations[0], Location::Constant { value: 0, .. }));
  assert!(matches!(record.locations[1], Location::Constant { value: 0, .. }));
  assert!(matches!(record.locations[2], Location::Constant { value: 0, .. }));

  let non_const: Vec<_> = record
    .locations
    .iter()
    .filter(|l| !matches!(l, Location::Constant { .. } | Location::ConstIndex { .. }))
    .collect();
  assert_eq!(non_const.len(), 2);

  match non_const[0] {
    Location::Indirect { dwarf_reg, offset, size } => {
      assert_eq!(*dwarf_reg, 7, "expected RSP base register");
      assert_eq!(*offset, 0);
      assert_eq!(*size, 8);
    }
    other => panic!("expected Indirect location, got {other:?}"),
  }
  match non_const[1] {
    Location::Indirect { dwarf_reg, offset, size } => {
      assert_eq!(*dwarf_reg, 7, "expected RSP base register");
      assert_eq!(*offset, 8);
      assert_eq!(*size, 8);
    }
    other => panic!("expected Indirect location, got {other:?}"),
  }
}
