use object::{Object, ObjectSection};
use runtime_native::stackmaps::{Location, StackMap, StackMaps, StackSize};
#[cfg(target_arch = "x86_64")]
use runtime_native::arch::SafepointContext;
#[cfg(target_arch = "x86_64")]
use runtime_native::stackwalk::StackBounds;
#[cfg(target_arch = "x86_64")]
use runtime_native::stackwalk_fp::walk_gc_roots_from_safepoint_context;
#[cfg(target_arch = "x86_64")]
use runtime_native::{walk_gc_roots_from_fp, WalkError};
use std::fs;
use std::process::{Command, Stdio};

fn tool_available(tool: &str) -> bool {
  Command::new(tool)
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok()
}

fn run_success(mut cmd: Command) {
  let cmd_str = format!("{cmd:?}");
  let out = cmd.output().unwrap_or_else(|e| panic!("failed to run {cmd_str}: {e}"));
  if !out.status.success() {
    panic!(
      "command failed: {cmd_str}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
      out.status,
      String::from_utf8_lossy(&out.stdout),
      String::from_utf8_lossy(&out.stderr),
    );
  }
}

fn stackmaps_section_bytes_from_obj(obj_bytes: &[u8]) -> Vec<u8> {
  let obj = object::File::parse(obj_bytes).expect("failed to parse object file");
  let section = obj
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section");
  section
    .data()
    .expect("failed to read .llvm_stackmaps section bytes")
    .to_vec()
}

fn dyn_alloca_statepoint_ir(triple: &str) -> String {
  // Use `llvm.gcroot` so `opt -passes=rewrite-statepoints-for-gc` will generate an LLVM 18
  // `gc.statepoint` + stackmap record. Add a dynamic alloca to force `stack_size = -1`.
  format!(
    r#"
source_filename = "dyn_statepoint"
target triple = "{triple}"

declare void @llvm.gcroot(ptr, ptr)
declare void @safepoint()

define i64 @dyn_statepoint(ptr addrspace(1) %p1, ptr addrspace(1) %p2, i64 %n) gc "coreclr" {{
entry:
  %dyn = alloca i8, i64 %n, align 16
  store i8 0, ptr %dyn, align 1

  %root1 = alloca ptr addrspace(1), align 8
  %root2 = alloca ptr addrspace(1), align 8
  call void @llvm.gcroot(ptr %root1, ptr null)
  call void @llvm.gcroot(ptr %root2, ptr null)

  store ptr addrspace(1) %p1, ptr %root1, align 8
  store ptr addrspace(1) %p2, ptr %root2, align 8

  %v1 = load ptr addrspace(1), ptr %root1, align 8
  %v2 = load ptr addrspace(1), ptr %root2, align 8

  call void @safepoint()

  %i1 = ptrtoint ptr addrspace(1) %v1 to i64
  %i2 = ptrtoint ptr addrspace(1) %v2 to i64
  %sum = add i64 %i1, %i2
  ret i64 %sum
}}
"#
  )
}

#[test]
fn dynamic_alloca_function_reports_unknown_stack_size() {
  for tool in ["opt-18", "llc-18"] {
    if !tool_available(tool) {
      eprintln!("skipping: {tool} not available in PATH");
      return;
    }
  }

  let tmp = tempfile::tempdir().expect("create tempdir");

  for (triple, cpu) in [
    ("x86_64-unknown-linux-gnu", "x86-64"),
    ("aarch64-unknown-linux-gnu", "generic"),
  ] {
    let ir_path = tmp.path().join(format!("dyn_statepoint_{cpu}.ll"));
    let rewritten = tmp.path().join(format!("dyn_statepoint_{cpu}.rewrite.ll"));
    let obj = tmp.path().join(format!("dyn_statepoint_{cpu}.o"));

    fs::write(&ir_path, dyn_alloca_statepoint_ir(triple)).expect("write IR");

    let mut opt = Command::new("opt-18");
    opt
      .arg(format!("-mtriple={triple}"))
      .arg("-passes=rewrite-statepoints-for-gc")
      .arg("-S")
      .arg(&ir_path)
      .arg("-o")
      .arg(&rewritten);
    run_success(opt);

    let mut llc = Command::new("llc-18");
    llc
      .arg("-O0")
      .arg("-filetype=obj")
      .arg(format!("-mtriple={triple}"))
      .arg(format!("-mcpu={cpu}"))
      .arg("-frame-pointer=all")
      .arg(&rewritten)
      .arg("-o")
      .arg(&obj);
    run_success(llc);

    let obj_bytes = fs::read(&obj).expect("read object");
    let stackmap_bytes = stackmaps_section_bytes_from_obj(&obj_bytes);

    let raw = StackMap::parse(&stackmap_bytes).expect("parse stackmap");
    assert_eq!(raw.functions.len(), 1);
    assert_eq!(
      raw.functions[0].stack_size,
      StackSize::Unknown,
      "{triple} dyn alloca should produce stack_size = Unknown"
    );

    // Ensure we still have at least one addressable spill slot recorded.
    assert!(
      raw.records
        .iter()
        .any(|rec| rec.locations.iter().any(|loc| matches!(loc, Location::Indirect { .. }))),
      "expected at least one Indirect location in dyn alloca stackmap for {triple}"
    );

    // The indexed view verifies statepoint conventions using the host DWARF register numbers.
    // That's only meaningful for the native target, but we still want the AArch64 object build to
    // run in CI.
    let is_native_arch = (cfg!(target_arch = "x86_64") && triple.starts_with("x86_64"))
      || (cfg!(target_arch = "aarch64") && triple.starts_with("aarch64"));
    if is_native_arch {
      let indexed = StackMaps::parse(&stackmap_bytes).expect("parse + index stackmaps");
      let (_pc, callsite) = indexed.iter().next().expect("non-empty");
      assert_eq!(callsite.stack_size, StackSize::Unknown);
    }
  }
}

#[cfg(target_arch = "x86_64")]
mod stackwalk_unknown_stack_size {
  use super::*;
  use runtime_native::statepoints::{X86_64_DWARF_REG_FP, X86_64_DWARF_REG_SP};

  fn align_up(v: usize, align: usize) -> usize {
    (v + (align - 1)) & !(align - 1)
  }

  unsafe fn write_u64(addr: usize, val: u64) {
    (addr as *mut u64).write_unaligned(val);
  }

  fn build_stackmaps_unknown_stack_size_root(dwarf_reg: u16, offset: i32) -> Vec<u8> {
    // Minimal stackmap section containing one statepoint record with one GC root pair.
    let mut out = Vec::new();

    // Header.
    out.push(3); // version
    out.push(0); // reserved0
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved1
    out.extend_from_slice(&1u32.to_le_bytes()); // num_functions
    out.extend_from_slice(&0u32.to_le_bytes()); // num_constants
    out.extend_from_slice(&1u32.to_le_bytes()); // num_records

    // One function record: stack_size = -1 (unknown).
    out.extend_from_slice(&0u64.to_le_bytes()); // address
    out.extend_from_slice(&u64::MAX.to_le_bytes()); // stack_size
    out.extend_from_slice(&1u64.to_le_bytes()); // record_count

    // One record at pc=0x10.
    out.extend_from_slice(&0xabcdef00u64.to_le_bytes()); // patchpoint_id (matches verifier's statepoint id)
    out.extend_from_slice(&0x10u32.to_le_bytes()); // instruction_offset
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&5u16.to_le_bytes()); // num_locations = 3 header + 1 pair

    // 3 leading constants (statepoint header).
    for _ in 0..3 {
      out.extend_from_slice(&[4, 0]); // Constant, reserved
      out.extend_from_slice(&8u16.to_le_bytes()); // size
      out.extend_from_slice(&0u16.to_le_bytes()); // dwarf_reg
      out.extend_from_slice(&0u16.to_le_bytes()); // reserved
      out.extend_from_slice(&0i32.to_le_bytes()); // small const
    }

    // base: Indirect [R#dwarf_reg + offset]
    out.extend_from_slice(&[3, 0]); // Indirect, reserved
    out.extend_from_slice(&8u16.to_le_bytes()); // size
    out.extend_from_slice(&dwarf_reg.to_le_bytes()); // dwarf_reg
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&offset.to_le_bytes()); // offset

    // derived: same as base (not a derived pointer)
    out.extend_from_slice(&[3, 0]); // Indirect, reserved
    out.extend_from_slice(&8u16.to_le_bytes()); // size
    out.extend_from_slice(&dwarf_reg.to_le_bytes()); // dwarf_reg
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&offset.to_le_bytes()); // offset

    // Align to 8.
    while out.len() % 8 != 0 {
      out.push(0);
    }

    // LiveOuts (none): padding + NumLiveOuts.
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());

    // Align to 8.
    while out.len() % 8 != 0 {
      out.push(0);
    }

    out
  }

  #[test]
  fn top_frame_sp_relative_root_requires_captured_sp_when_stack_size_is_unknown() {
    let bytes = build_stackmaps_unknown_stack_size_root(X86_64_DWARF_REG_SP, 0);
    let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps");
    let (callsite_ra, _callsite) = stackmaps.iter().next().expect("non-empty");

    let mut stack = vec![0u8; 512];
    let base = stack.as_mut_ptr() as usize;
    let caller_fp = align_up(base + 256, 16);

    unsafe {
      // Terminal managed frame.
      write_u64(caller_fp + 0, 0);
      write_u64(caller_fp + 8, 0);
    }

    let ctx = SafepointContext {
      fp: caller_fp,
      ip: callsite_ra as usize,
      sp: 0,
      ..Default::default()
    };

    let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();
    let res = unsafe { walk_gc_roots_from_safepoint_context(&ctx, Some(bounds), &stackmaps, |_| {}) };
    assert!(
      matches!(res, Err(WalkError::UnknownStackSize { .. })),
      "expected UnknownStackSize, got {res:?}"
    );
  }

  #[test]
  fn top_frame_sp_relative_root_works_when_sp_is_provided() {
    let bytes = build_stackmaps_unknown_stack_size_root(X86_64_DWARF_REG_SP, 0);
    let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps");
    let (callsite_ra, _callsite) = stackmaps.iter().next().expect("non-empty");

    let mut stack = vec![0u8; 512];
    let base = stack.as_mut_ptr() as usize;
    let caller_fp = align_up(base + 256, 16);
    let caller_sp = align_up(base + 128, 16);

    // Slot is `[SP + 0]`.
    unsafe {
      write_u64(caller_sp, 0x1111_2222_3333_4444);
      // Terminal managed frame.
      write_u64(caller_fp + 0, 0);
      write_u64(caller_fp + 8, 0);
    }

    let ctx = SafepointContext {
      fp: caller_fp,
      ip: callsite_ra as usize,
      sp: caller_sp,
      ..Default::default()
    };

    let mut visited: Vec<usize> = Vec::new();
    let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();
    unsafe {
      walk_gc_roots_from_safepoint_context(&ctx, Some(bounds), &stackmaps, |slot| {
        visited.push(slot as usize);
      })
      .expect("walk");
    }

    visited.sort_unstable();
    assert_eq!(visited, vec![caller_sp]);
  }

  #[test]
  fn fp_relative_root_does_not_require_stack_size_or_sp() {
    // Place the root at `[FP - 16]` so it doesn't overlap the frame record.
    let bytes = build_stackmaps_unknown_stack_size_root(X86_64_DWARF_REG_FP, -16);
    let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps");
    let (callsite_ra, _callsite) = stackmaps.iter().next().expect("non-empty");

    let mut stack = vec![0u8; 512];
    let base = stack.as_mut_ptr() as usize;
    let start_fp = align_up(base + 128, 16);
    let caller_fp = align_up(base + 256, 16);
    let slot_addr = caller_fp - 16;

    unsafe {
      write_u64(slot_addr, 0x1111_2222_3333_4444);
      write_u64(start_fp + 0, caller_fp as u64);
      write_u64(start_fp + 8, callsite_ra);
      // Terminal managed frame.
      write_u64(caller_fp + 0, 0);
      write_u64(caller_fp + 8, 0);
    }

    let mut visited: Vec<usize> = Vec::new();
    let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();
    unsafe {
      walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |slot| {
        visited.push(slot as usize);
      })
      .expect("walk");
    }
    visited.sort_unstable();
    assert_eq!(visited, vec![slot_addr]);
  }
}
