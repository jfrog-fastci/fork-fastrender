#![cfg(target_arch = "x86_64")]

use runtime_native::stackwalk::StackBounds;
use runtime_native::{walk_gc_roots_from_fp, StackMaps};

#[test]
fn rsp_is_reconstructed_from_fp_and_stack_size_for_rsp_based_locations() {
  // Minimal stackmap section containing one callsite record where GC root locations are reported
  // as Indirect [RSP + off]. This is the common case even when frame pointers are enabled.
  //
  // We specifically model the common prologue:
  //   push rbp
  //   mov  rbp, rsp
  //   sub  rsp, 0x10
  //
  // In LLVM StackMaps v3 (LLVM 18), the per-function `stack_size` includes the pushed RBP, so:
  //   stack_size = 0x10 (locals) + 8 (saved rbp) = 24
  //
  // And a slot at -0x10(%rbp) is described as [RSP + 0] in the stackmap because:
  //   rsp_at_callsite = rbp + 8 - stack_size = rbp - 0x10
  let bytes = build_stackmaps_with_rsp_slots();
  let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps");
  let (callsite_ra, callsite) = stackmaps.iter().next().expect("callsite");
  assert_eq!(callsite.stack_size, 24);

  // Fake stack memory (addresses increase upward; stack grows downward).
  let mut stack = vec![0u8; 256];
  let base = stack.as_mut_ptr() as usize;
  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();

  // Layout a single managed frame with:
  //   [fp + 0]  = saved previous fp
  //   [fp + 8]  = return address
  //   [fp - 16] = local slot #0
  //   [fp - 8]  = local slot #1
  //
  // Start walking from a runtime frame that "returns into" the managed frame at `callsite_ra`.
  let caller_fp = align_up(base + 0x80, 16);
  // Model the relationship between the callee frame pointer and the caller's stack pointer at the
  // callsite:
  // - x86_64 `call` pushes the return address (8 bytes),
  // - the callee prologue pushes the caller's RBP (8 bytes),
  // so `callee_fp = caller_sp_callsite - 16`.
  //
  // With `stack_size = 24`, `caller_sp_callsite = caller_fp + 8 - stack_size = caller_fp - 16`, so
  // `callee_fp = caller_fp - 32`.
  let start_fp = caller_fp - 0x20;
  assert!(caller_fp > start_fp);

  let slot0 = caller_fp - 0x10;
  let slot1 = caller_fp - 0x8;

  unsafe {
    // runtime frame -> caller frame
    write_u64(start_fp + 0, caller_fp as u64);
    write_u64(start_fp + 8, callsite_ra);

    // caller frame -> null (end of chain)
    write_u64(caller_fp + 0, 0);
    write_u64(caller_fp + 8, 0);

    // Fill the two local slots with dummy pointer values.
    write_u64(slot0, 0x1111_1111_1111_1111);
    write_u64(slot1, 0x2222_2222_2222_2222);
  }

  let mut visited: Vec<usize> = Vec::new();
  unsafe {
    walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |slot| {
      visited.push(slot as usize);
    })
    .expect("walk");
  }

  visited.sort_unstable();
  assert_eq!(visited, vec![slot0, slot1]);
}

fn build_stackmaps_with_rsp_slots() -> Vec<u8> {
  // Minimal StackMap v3 blob:
  // - 1 function record
  // - 1 callsite record
  // - record contains a statepoint-like layout (3 constant header locations) and two `(base,derived)`
  //   GC root pairs, both spilled to stack slots reported as Indirect [RSP + off].
  let mut out = Vec::new();

  // Header.
  out.push(3); // version
  out.push(0); // reserved0
  out.extend_from_slice(&0u16.to_le_bytes()); // reserved1
  out.extend_from_slice(&1u32.to_le_bytes()); // num_functions
  out.extend_from_slice(&0u32.to_le_bytes()); // num_constants
  out.extend_from_slice(&1u32.to_le_bytes()); // num_records

  // One function record.
  out.extend_from_slice(&0x1000u64.to_le_bytes()); // address
  out.extend_from_slice(&24u64.to_le_bytes()); // stack_size
  out.extend_from_slice(&1u64.to_le_bytes()); // record_count

  // One record.
  out.extend_from_slice(&0xabcdef00u64.to_le_bytes()); // patchpoint_id
  out.extend_from_slice(&0x10u32.to_le_bytes()); // instruction_offset
  out.extend_from_slice(&0u16.to_le_bytes()); // reserved
  out.extend_from_slice(&7u16.to_le_bytes()); // num_locations (3 header consts + 4 GC locs)

  // 3 leading constants (statepoint header).
  for _ in 0..3 {
    out.extend_from_slice(&[4, 0]); // Constant, reserved
    out.extend_from_slice(&8u16.to_le_bytes()); // size
    out.extend_from_slice(&0u16.to_le_bytes()); // dwarf_reg
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&0i32.to_le_bytes()); // small const
  }

  // base0/derived0: Indirect [RSP + 0]
  for _ in 0..2 {
    out.extend_from_slice(&[3, 0]); // Indirect, reserved
    out.extend_from_slice(&8u16.to_le_bytes()); // size
    out.extend_from_slice(&7u16.to_le_bytes()); // dwarf_reg (x86_64 RSP)
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&0i32.to_le_bytes()); // offset
  }

  // base1/derived1: Indirect [RSP + 8]
  for _ in 0..2 {
    out.extend_from_slice(&[3, 0]); // Indirect, reserved
    out.extend_from_slice(&8u16.to_le_bytes()); // size
    out.extend_from_slice(&7u16.to_le_bytes()); // dwarf_reg (x86_64 RSP)
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&8i32.to_le_bytes()); // offset
  }

  // Align to 8 before live-out header.
  while out.len() % 8 != 0 {
    out.push(0);
  }

  // Live-out header: padding + num_live_outs=0.
  out.extend_from_slice(&0u16.to_le_bytes());
  out.extend_from_slice(&0u16.to_le_bytes());

  // Record ends aligned to 8.
  while out.len() % 8 != 0 {
    out.push(0);
  }

  out
}

fn align_up(v: usize, align: usize) -> usize {
  (v + (align - 1)) & !(align - 1)
}

unsafe fn write_u64(addr: usize, val: u64) {
  (addr as *mut u64).write_unaligned(val);
}
