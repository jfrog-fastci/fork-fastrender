#![cfg(all(target_arch = "x86_64", target_os = "linux"))]

use object::{Object, ObjectSection};
use runtime_native::stackmaps::StackSize;
use runtime_native::stackwalk::StackBounds;
use runtime_native::statepoints::{StatepointRecord, X86_64_DWARF_REG_SP};
use runtime_native::{walk_gc_roots_from_fp, StackMaps};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn tool_available(tool: &str) -> bool {
  Command::new(tool)
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok()
}

fn run_success(cmd: &mut Command) {
  let out = cmd.output().unwrap_or_else(|e| panic!("failed to run {cmd:?}: {e}"));
  if !out.status.success() {
    panic!(
      "command failed: {cmd:?}\nstdout:\n{}\nstderr:\n{}",
      String::from_utf8_lossy(&out.stdout),
      String::from_utf8_lossy(&out.stderr)
    );
  }
}

fn stackmaps_section_bytes_from_obj(obj_bytes: &[u8]) -> Vec<u8> {
  let obj = object::File::parse(obj_bytes).expect("parse object file");
  let section = obj
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section");
  section.data().expect("read .llvm_stackmaps bytes").to_vec()
}

fn write_ir_with_stack_args(triple: &str) -> String {
  // Force a per-call stack adjustment by giving the callee enough integer arguments to spill some
  // args onto the stack (SysV x86_64 has 6 integer/pointer arg registers).
  //
  // Keep one GC pointer live across the call so `rewrite-statepoints-for-gc` will produce a
  // `gc.statepoint` with at least one `gc.relocate` pair.
  format!(
    r#"
target triple = "{triple}"

declare void @callee(i64, i64, i64, i64, i64, i64, i64, i64, ptr addrspace(1))

define ptr addrspace(1) @foo(ptr addrspace(1) %p) gc "coreclr" {{
entry:
  call void @callee(
    i64 1, i64 2, i64 3, i64 4, i64 5, i64 6, i64 7, i64 8,
    ptr addrspace(1) %p
  )
  ret ptr addrspace(1) %p
}}
"#
  )
}

fn build_obj(tmp: &Path) -> PathBuf {
  let input_ll = tmp.join("input.ll");
  let input_bc = tmp.join("input.bc");
  let rewritten_bc = tmp.join("rewritten.bc");
  let obj = tmp.join("out.o");

  fs::write(&input_ll, write_ir_with_stack_args("x86_64-unknown-linux-gnu")).expect("write IR");

  run_success(
    Command::new("llvm-as-18")
      .arg(&input_ll)
      .arg("-o")
      .arg(&input_bc),
  );

  run_success(
    Command::new("opt-18")
      .arg("-passes=rewrite-statepoints-for-gc")
      .arg(&input_bc)
      .arg("-o")
      .arg(&rewritten_bc),
  );

  run_success(
    Command::new("llc-18")
      .arg("-O0")
      .arg("-filetype=obj")
      .arg("-frame-pointer=all")
      // runtime-native requires statepoint roots to be spilled to stack slots.
      .arg("--fixup-allow-gcptr-in-csr=false")
      .arg("--fixup-max-csr-statepoints=0")
      .arg(&rewritten_bc)
      .arg("-o")
      .arg(&obj),
  );

  obj
}

fn align_up(v: usize, align: usize) -> usize {
  debug_assert!(align.is_power_of_two());
  (v + (align - 1)) & !(align - 1)
}

unsafe fn write_u64(addr: usize, val: u64) {
  (addr as *mut u64).write_unaligned(val);
}

#[test]
fn sp_relative_roots_use_callsite_sp_from_callee_fp_not_stack_size() {
  for tool in ["llvm-as-18", "opt-18", "llc-18"] {
    if !tool_available(tool) {
      eprintln!("skipping: {tool} not available in PATH");
      return;
    }
  }

  let tmp = tempfile::tempdir().expect("create tempdir");
  let obj_path = build_obj(tmp.path());
  let obj_bytes = fs::read(&obj_path).expect("read object");
  let stackmap_bytes = stackmaps_section_bytes_from_obj(&obj_bytes);

  let stackmaps = StackMaps::parse(&stackmap_bytes).expect("parse + index stackmaps");

  // Pick the first statepoint callsite with at least one SP-relative GC root location.
  let (callsite_ra, callsite, sp_offset) = stackmaps
    .iter()
    .find_map(|(pc, callsite)| {
      let sp = StatepointRecord::new(callsite.record).ok()?;
      if sp.gc_pair_count() != 1 {
        return None;
      }
      let pair0 = sp.gc_pairs().first()?;
      if pair0.base != pair0.derived {
        return None;
      }
      match &pair0.base {
        runtime_native::stackmaps::Location::Indirect {
          dwarf_reg,
          offset,
          ..
        } if *dwarf_reg == X86_64_DWARF_REG_SP => Some((pc, callsite, *offset)),
        _ => None,
      }
    })
    .expect("expected a statepoint with an SP-relative GC root");

  let StackSize::Known(stack_size) = callsite.stack_size else {
    panic!("expected known stack_size (test targets outgoing stack args, not dynamic alloca)");
  };
  let stack_size: usize = stack_size.try_into().expect("stack_size fits usize");

  // Synthetic stack memory (stack grows down, addresses increase upward).
  let mut stack = vec![0u8; 4096];
  let base = stack.as_mut_ptr() as usize;
  let hi = base + stack.len();
  let bounds = StackBounds::new(base as u64, hi as u64).unwrap();

  // Choose a caller frame pointer (managed frame) and derive:
  // - `sp_fixed`: what a stack_size-based reconstruction would compute
  // - `caller_sp_callsite`: the *actual* callsite SP base used by stackmaps, which can differ due
  //   to per-call stack adjustments (outgoing stack args).
  //
  // We model a non-zero per-call adjustment by aligning `caller_sp_callsite` down from
  // `sp_fixed - 32` to keep the SP 16-byte aligned.
  let caller_fp = align_up(base + 0x900, 16);
  assert!(caller_fp + 16 <= hi, "caller FP record must be in bounds");

  let sp_fixed = caller_fp
    .checked_add(8)
    .and_then(|v| v.checked_sub(stack_size))
    .expect("sp_fixed underflow");
  let caller_sp_callsite = align_up(sp_fixed.saturating_sub(32), 16);
  let call_adjust = sp_fixed
    .checked_sub(caller_sp_callsite)
    .expect("call_adjust underflow");
  assert!(call_adjust > 0, "expected a non-zero per-call adjustment");

  // For x86_64 frame-pointer walking:
  //   caller_sp_callsite = callee_fp + 16
  let callee_fp = caller_sp_callsite - 16;
  assert_eq!(callee_fp % 16, 0, "callee FP must be 16-byte aligned");
  assert!(callee_fp >= base && callee_fp + 16 <= hi, "callee FP record must be in bounds");
  assert!(caller_fp > callee_fp, "FP chain must be monotonic");

  let slot_addr = (caller_sp_callsite as i128 + sp_offset as i128) as usize;
  let wrong_slot_addr = (sp_fixed as i128 + sp_offset as i128) as usize;
  assert_ne!(
    slot_addr, wrong_slot_addr,
    "expected per-call adjustment to change the computed slot address"
  );
  assert!(
    slot_addr >= base && slot_addr + 8 <= hi,
    "root slot addr must be in bounds"
  );
  assert!(
    wrong_slot_addr >= base && wrong_slot_addr + 8 <= hi,
    "stack_size-derived (wrong) slot addr must be in bounds"
  );
  assert_eq!(slot_addr % 8, 0, "root slot must be 8-byte aligned");
  assert_eq!(wrong_slot_addr % 8, 0, "wrong slot must be 8-byte aligned");

  // Frame records:
  // - callee frame -> caller frame (return address is the callsite PC)
  // - caller frame -> end
  unsafe {
    write_u64(callee_fp + 0, caller_fp as u64);
    write_u64(callee_fp + 8, callsite_ra);
    write_u64(caller_fp + 0, 0);
    write_u64(caller_fp + 8, 0);

    // Seed the correct slot with a sentinel and ensure the stack_size-derived slot differs.
    write_u64(slot_addr, 0x1111_2222_3333_4444);
    write_u64(wrong_slot_addr, 0x5555_6666_7777_8888);
  }

  let mut visited: Vec<usize> = Vec::new();
  unsafe {
    walk_gc_roots_from_fp(callee_fp as u64, Some(bounds), &stackmaps, |slot| {
      visited.push(slot as usize);
    })
    .expect("walk_gc_roots_from_fp");
  }

  visited.sort_unstable();
  visited.dedup();

  assert!(
    visited.contains(&slot_addr),
    "expected walker to visit correct slot addr (callee_fp+16 base)\n\
     callsite_ra=0x{callsite_ra:x}\n\
     stack_size={stack_size}\n\
     sp_offset={sp_offset}\n\
     sp_fixed=0x{sp_fixed:x}\n\
     caller_sp_callsite=0x{caller_sp_callsite:x}\n\
     correct_slot=0x{slot_addr:x}\n\
     wrong_slot=0x{wrong_slot_addr:x}\n\
     visited={visited:x?}"
  );

  // Assert the visited slot contains our sentinel.
  let mut saw_sentinel = false;
  for &addr in &visited {
    if addr == slot_addr {
      let val = unsafe { (addr as *const u64).read_unaligned() };
      assert_eq!(val, 0x1111_2222_3333_4444);
      saw_sentinel = true;
    }
  }
  assert!(saw_sentinel);
}
