#![cfg(target_arch = "x86_64")]

use runtime_native::arch::SafepointContext;
use runtime_native::stackwalk::StackBounds;
use runtime_native::stackwalk_fp::{
  walk_gc_root_pairs_from_safepoint_context, walk_gc_roots_from_safepoint_context,
};
use runtime_native::StackMaps;

/// Regression test: LLVM stackmap `Indirect [RSP + off]` locations are based on the
/// **post-call** stack pointer (at the stackmap record PC), but `call` pushes an
/// 8-byte return address on x86_64.
///
/// When a thread is stopped inside the safepoint callee, the callee-entry `rsp`
/// points at the return address and is therefore 8 bytes lower than the stackmap
/// base. The runtime must publish `sp = rsp_entry + 8` for correct root slot
/// evaluation.
#[test]
fn sp_relative_stackmap_locations_use_post_call_rsp() {
  // Stackmap with one callsite record and one spilled root at [RSP + ROOT_OFF].
  const ROOT_OFF: i32 = 16;
  let stackmaps =
    StackMaps::parse(&build_stackmaps_one_sp_root(ROOT_OFF)).expect("parse stackmaps");
  let (callsite_ra, _callsite) = stackmaps.iter().next().expect("one callsite");

  // Fake stack memory.
  let mut stack = vec![0u8; 256];
  let base = stack.as_mut_ptr() as usize;

  // Simulate entering a safepoint callee:
  //   rsp_entry -> [return address]
  let sp_entry = align_up(base + 64, 8);
  unsafe {
    write_u64(sp_entry, callsite_ra);
  }

  // Stackmap semantics: base SP is *post-call* (return address popped).
  let sp_post_call = sp_entry + 8;

  // Root slot is relative to that post-call SP.
  let slot_addr = sp_post_call + (ROOT_OFF as usize);
  let obj = Box::into_raw(Box::new(0u8)) as u64;
  unsafe {
    write_u64(slot_addr, obj);
  }

  // Minimal terminal frame record so `walk_gc_roots_from_fp` returns immediately
  // after processing the top frame.
  let fp = align_up(base + 160, 8);
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

  let mut visited: Vec<usize> = Vec::new();
  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();
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

#[test]
fn sp_relative_stackmap_locations_use_post_call_rsp_for_pair_walker() {
  // Stackmap with one callsite record and one spilled root at [RSP + ROOT_OFF].
  const ROOT_OFF: i32 = 16;
  let stackmaps =
    StackMaps::parse(&build_stackmaps_one_sp_root(ROOT_OFF)).expect("parse stackmaps");
  let (callsite_ra, _callsite) = stackmaps.iter().next().expect("one callsite");

  // Fake stack memory.
  let mut stack = vec![0u8; 256];
  let base = stack.as_mut_ptr() as usize;

  // Simulate entering a safepoint callee:
  //   rsp_entry -> [return address]
  let sp_entry = align_up(base + 64, 8);
  unsafe {
    write_u64(sp_entry, callsite_ra);
  }

  // Stackmap semantics: base SP is *post-call* (return address popped).
  let sp_post_call = sp_entry + 8;

  // Root slot is relative to that post-call SP.
  let slot_addr = sp_post_call + (ROOT_OFF as usize);
  let obj = Box::into_raw(Box::new(0u8)) as u64;
  unsafe {
    write_u64(slot_addr, obj);
  }

  // Minimal terminal frame record so `walk_gc_root_pairs_from_fp` returns immediately
  // after processing the top frame.
  let fp = align_up(base + 160, 8);
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

  let mut visited: Vec<(usize, usize)> = Vec::new();
  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();
  unsafe {
    walk_gc_root_pairs_from_safepoint_context(&ctx, Some(bounds), &stackmaps, |ra, pairs| {
      assert_eq!(ra, callsite_ra);
      for &(base_slot, derived_slot) in pairs {
        visited.push((base_slot as usize, derived_slot as usize));
      }
    })
    .expect("walk");
  }

  visited.sort_unstable();
  visited.dedup();
  assert_eq!(visited, vec![(slot_addr, slot_addr)]);
}

fn build_stackmaps_one_sp_root(offset: i32) -> Vec<u8> {
  // Minimal StackMap v3 section containing a single function record and a single
  // callsite record. The record is shaped like an LLVM 18 statepoint:
  //
  // - 3 leading constants (callconv, flags, deopt_count)
  // - followed by one (base, derived) gc-live pair
  //
  // For this test we record exactly one spilled GC root slot at [SP + offset]
  // and set base==derived to avoid derived-pointer handling.
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
  out.extend_from_slice(&40u64.to_le_bytes()); // stack_size (arbitrary >= FP_RECORD_SIZE)
  out.extend_from_slice(&1u64.to_le_bytes()); // record_count

  // One record.
  out.extend_from_slice(&0xabcdef00u64.to_le_bytes()); // patchpoint_id
  out.extend_from_slice(&0x10u32.to_le_bytes()); // instruction_offset (=> callsite PC=0x1010)
  out.extend_from_slice(&0u16.to_le_bytes()); // reserved
  out.extend_from_slice(&5u16.to_le_bytes()); // num_locations (3 header + 2 gc pair)

  // 3 leading constants (statepoint header). Values are irrelevant for this test.
  for _ in 0..3 {
    out.extend_from_slice(&[4, 0]); // Constant, reserved
    out.extend_from_slice(&8u16.to_le_bytes()); // size
    out.extend_from_slice(&0u16.to_le_bytes()); // dwarf_reg
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&0i32.to_le_bytes()); // small const
  }

  // base: Indirect [SP + offset]
  out.extend_from_slice(&[3, 0]); // Indirect, reserved
  out.extend_from_slice(&8u16.to_le_bytes()); // size
  out.extend_from_slice(&7u16.to_le_bytes()); // dwarf_reg (x86_64 SP)
  out.extend_from_slice(&0u16.to_le_bytes()); // reserved
  out.extend_from_slice(&offset.to_le_bytes());

  // derived: same as base (not a derived pointer).
  out.extend_from_slice(&[3, 0]); // Indirect, reserved
  out.extend_from_slice(&8u16.to_le_bytes()); // size
  out.extend_from_slice(&7u16.to_le_bytes()); // dwarf_reg (x86_64 SP)
  out.extend_from_slice(&0u16.to_le_bytes()); // reserved
  out.extend_from_slice(&offset.to_le_bytes());

  // Align to 8 for the live-out header.
  while out.len() % 8 != 0 {
    out.push(0);
  }

  // LiveOuts (none): padding + num_live_outs.
  out.extend_from_slice(&0u16.to_le_bytes()); // padding
  out.extend_from_slice(&0u16.to_le_bytes()); // num_live_outs

  // Align to 8 after live-outs.
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
