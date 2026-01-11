#![cfg(all(target_arch = "x86_64", target_os = "linux"))]

use object::{Object, ObjectSection};
use runtime_native::arch::SafepointContext;
use runtime_native::stackmaps::Location;
use runtime_native::stackwalk::StackBounds;
use runtime_native::stackwalk_fp::walk_gc_roots_from_safepoint_context;
use runtime_native::statepoints::StatepointRecord;
use runtime_native::StackMaps;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tool_available(bin: &str) -> bool {
  Command::new(bin)
    .arg("--version")
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .map(|st| st.success())
    .unwrap_or(false)
}

fn run_success(cmd: &mut Command) {
  let out = cmd
    .output()
    .unwrap_or_else(|e| panic!("failed to run {cmd:?}: {e}"));
  if !out.status.success() {
    panic!(
      "command failed: {cmd:?}\nstdout:\n{}\nstderr:\n{}",
      String::from_utf8_lossy(&out.stdout),
      String::from_utf8_lossy(&out.stderr)
    );
  }
}

fn stackmaps_section_bytes(obj_bytes: &[u8]) -> &[u8] {
  let obj = object::File::parse(obj_bytes).expect("parse object");
  let section = obj
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section");
  section.data().expect("read .llvm_stackmaps bytes")
}

fn build_obj(tmp: &Path) -> PathBuf {
  let input_ll = tmp.join("input.ll");
  let rewritten_ll = tmp.join("rewritten.ll");
  let obj = tmp.join("out.o");

  // Force outgoing stack arguments: the 7th+ integer args are passed on the stack on x86_64 SysV.
  let ll = r#"
target triple = "x86_64-unknown-linux-gnu"

declare void @callee(i64, i64, i64, i64, i64, i64, ptr addrspace(1), i64, i64)

define ptr addrspace(1) @foo(ptr addrspace(1) %p) gc "coreclr" {
entry:
  call void @callee(i64 1, i64 2, i64 3, i64 4, i64 5, i64 6, ptr addrspace(1) %p, i64 8, i64 9)
  ret ptr addrspace(1) %p
}
"#;
  fs::write(&input_ll, ll).unwrap();

  run_success(
    Command::new("opt-18")
      .arg("-S")
      .arg("-passes=rewrite-statepoints-for-gc")
      .arg(&input_ll)
      .arg("-o")
      .arg(&rewritten_ll),
  );

  run_success(
    Command::new("llc-18")
      .arg("-O0")
      .arg("--frame-pointer=all")
      // Force spills for gc-live values; frame-pointer-only stack walking cannot reconstruct register roots.
      .arg("--fixup-allow-gcptr-in-csr=false")
      .arg("--fixup-max-csr-statepoints=0")
      .arg("-filetype=obj")
      .arg(&rewritten_ll)
      .arg("-o")
      .arg(&obj),
  );

  obj
}

/// LLVM-backed regression test for `SafepointContext.sp` semantics on x86_64.
///
/// LLVM StackMaps `Indirect [SP + off]` locations are based on the *caller* SP at the stackmap
/// record PC (the instruction after the call returns). On x86_64, the callee-entry `rsp` points at
/// the return address pushed by `call`, so the runtime must publish `sp = sp_entry + 8` when a
/// thread is stopped inside the safepoint callee.
#[test]
fn llvm_statepoint_stackmap_sp_is_post_call_rsp() {
  if !tool_available("opt-18") || !tool_available("llc-18") {
    // Allow this test to run on hosts without LLVM installed.
    return;
  }

  let tmp = tempfile::tempdir().unwrap();
  let obj_path = build_obj(tmp.path());
  let obj_bytes = fs::read(&obj_path).unwrap();

  let stackmaps =
    StackMaps::parse(stackmaps_section_bytes(&obj_bytes)).expect("parse .llvm_stackmaps");
  let (callsite_ra, callsite) = stackmaps.iter().next().expect("one callsite");
  let statepoint = StatepointRecord::new(callsite.record).expect("decode statepoint layout");
  assert_eq!(statepoint.gc_pair_count(), 1, "expected exactly one gc-live pair");

  let Location::Indirect {
    dwarf_reg,
    offset,
    ..
  } = &statepoint.gc_pairs()[0].base
  else {
    panic!("expected Indirect [SP + off] root location");
  };
  assert_eq!(*dwarf_reg, 7, "expected x86_64 DWARF SP (RSP)");

  // Fake stack memory.
  let mut stack = vec![0u8; 512];
  let base = stack.as_mut_ptr() as usize;

  // Simulate entering a safepoint callee:
  //   rsp_entry -> [return address]
  let sp_entry = align_up(base + 64, 8);
  unsafe {
    write_u64(sp_entry, callsite_ra);
  }

  // Stackmap SP semantics: base SP is post-call (return address popped).
  let sp_post_call = sp_entry + 8;

  // Root slot is relative to that post-call SP.
  let slot_addr = add_signed_u64(sp_post_call as u64, *offset).expect("slot addr") as usize;
  let obj = Box::into_raw(Box::new(0u8)) as u64;
  unsafe {
    write_u64(slot_addr, obj);
  }

  // Minimal terminal frame record so the delegated FP-walker returns immediately.
  let fp = align_up(base + 256, 16);
  unsafe {
    write_u64(fp + 0, 0);
    write_u64(fp + 8, 0);
  }

  let ctx = SafepointContext {
    sp_entry,
    sp: sp_post_call,
    fp,
    ip: callsite_ra as usize,
  };

  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();
  let mut visited: Vec<usize> = Vec::new();
  unsafe {
    walk_gc_roots_from_safepoint_context(&ctx, Some(bounds), &stackmaps, |slot| {
      visited.push(slot as usize);
    })
    .expect("walk");
  }

  visited.sort_unstable();
  visited.dedup();
  assert_eq!(visited, vec![slot_addr]);
}

fn align_up(v: usize, align: usize) -> usize {
  (v + (align - 1)) & !(align - 1)
}

fn add_signed_u64(base: u64, offset: i32) -> Option<u64> {
  if offset >= 0 {
    base.checked_add(offset as u64)
  } else {
    base.checked_sub((-offset) as u64)
  }
}

unsafe fn write_u64(addr: usize, val: u64) {
  (addr as *mut u64).write_unaligned(val);
}

