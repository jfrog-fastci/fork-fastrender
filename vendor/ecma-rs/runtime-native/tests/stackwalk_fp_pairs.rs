#![cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]

use runtime_native::arch;
use runtime_native::stackmaps::StackMaps;
#[cfg(target_arch = "x86_64")]
use runtime_native::stackmaps::StackSize;
use runtime_native::stackwalk::StackBounds;
use runtime_native::statepoint_verify::LLVM_STATEPOINT_PATCHPOINT_ID;
use runtime_native::WalkError;

/// Minimal StackMap v3 blob:
/// - one function record
/// - one callsite record keyed by `instruction_offset`
/// - statepoint layout: 3 constant header locations + one (base, derived) pair
fn minimal_statepoint_stackmap(instruction_offset: u32, stack_size: u64) -> Vec<u8> {
  minimal_statepoint_stackmap_with_offsets(instruction_offset, stack_size, 0, 8)
}

/// Like [`minimal_statepoint_stackmap`], but allows customizing the `(base, derived)` offsets.
fn minimal_statepoint_stackmap_with_offsets(
  instruction_offset: u32,
  stack_size: u64,
  base_off: i32,
  derived_off: i32,
) -> Vec<u8> {
  fn push_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
  }
  fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
  }
  fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
  }
  fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
  }
  fn push_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_le_bytes());
  }
  fn align_to(out: &mut Vec<u8>, align: usize) {
    while out.len() % align != 0 {
      out.push(0);
    }
  }
  fn push_loc(out: &mut Vec<u8>, kind: u8, size: u16, dwarf_reg: u16, offset: i32) {
    push_u8(out, kind);
    push_u8(out, 0); // reserved0
    push_u16(out, size);
    push_u16(out, dwarf_reg);
    push_u16(out, 0); // reserved1
    push_i32(out, offset);
  }

  let mut bytes = Vec::new();

  // Header.
  push_u8(&mut bytes, 3); // version
  push_u8(&mut bytes, 0); // reserved0
  push_u16(&mut bytes, 0); // reserved1
  push_u32(&mut bytes, 1); // numFunctions
  push_u32(&mut bytes, 0); // numConstants
  push_u32(&mut bytes, 1); // numRecords

  // Function record.
  push_u64(&mut bytes, 0); // address
  push_u64(&mut bytes, stack_size); // stack_size (intentionally not used by the walker)
  push_u64(&mut bytes, 1); // record_count

  // Record.
  push_u64(&mut bytes, LLVM_STATEPOINT_PATCHPOINT_ID); // patchpoint_id (not used for statepoint detection)
  push_u32(&mut bytes, instruction_offset);
  push_u16(&mut bytes, 0); // reserved
  push_u16(&mut bytes, 5); // num_locations

  // 3 constant header locations (callconv, flags, deopt_count).
  push_loc(&mut bytes, 4, 8, 0, 0);
  push_loc(&mut bytes, 4, 8, 0, 0);
  push_loc(&mut bytes, 4, 8, 0, 0);

  // One (base, derived) pair: Indirect [SP + 0], Indirect [SP + 8].
  let sp_reg = runtime_native::stackwalk::DWARF_SP_REG;
  push_loc(&mut bytes, 3, 8, sp_reg, base_off);
  push_loc(&mut bytes, 3, 8, sp_reg, derived_off);

  // Align to 8 before live-out header.
  align_to(&mut bytes, 8);
  push_u16(&mut bytes, 0); // live-out padding
  push_u16(&mut bytes, 0); // num_live_outs
  align_to(&mut bytes, 8);

  bytes
}

/// Minimal StackMap v3 blob with a single record that is intentionally **not** a statepoint.
fn minimal_non_statepoint_stackmap(instruction_offset: u32, stack_size: u64) -> Vec<u8> {
  fn push_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
  }
  fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
  }
  fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
  }
  fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
  }
  fn push_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_le_bytes());
  }
  fn align_to(out: &mut Vec<u8>, align: usize) {
    while out.len() % align != 0 {
      out.push(0);
    }
  }
  fn push_loc(out: &mut Vec<u8>, kind: u8, size: u16, dwarf_reg: u16, offset: i32) {
    push_u8(out, kind);
    push_u8(out, 0); // reserved0
    push_u16(out, size);
    push_u16(out, dwarf_reg);
    push_u16(out, 0); // reserved1
    push_i32(out, offset);
  }

  let mut bytes = Vec::new();

  // Header.
  push_u8(&mut bytes, 3); // version
  push_u8(&mut bytes, 0); // reserved0
  push_u16(&mut bytes, 0); // reserved1
  push_u32(&mut bytes, 1); // numFunctions
  push_u32(&mut bytes, 0); // numConstants
  push_u32(&mut bytes, 1); // numRecords

  // Function record.
  push_u64(&mut bytes, 0); // address
  push_u64(&mut bytes, stack_size);
  push_u64(&mut bytes, 1); // record_count

  // Record header: 1 location (does not have the 3-constant prefix).
  push_u64(&mut bytes, 0x1234); // patchpoint_id
  push_u32(&mut bytes, instruction_offset);
  push_u16(&mut bytes, 0); // reserved
  push_u16(&mut bytes, 1); // num_locations

  // One Register location.
  let sp_reg = runtime_native::stackwalk::DWARF_SP_REG;
  push_loc(&mut bytes, 1, 8, sp_reg, 0);

  // Align to 8 before live-out header.
  align_to(&mut bytes, 8);
  push_u16(&mut bytes, 0); // live-out padding
  push_u16(&mut bytes, 0); // num_live_outs
  align_to(&mut bytes, 8);

  bytes
}

#[repr(align(16))]
struct AlignedStack<const N: usize>([usize; N]);

#[test]
fn root_pairs_use_callee_fp_callsite_sp_not_stack_size() {
  // Create a synthetic stack with two frames:
  // - callee_fp: "runtime" frame (current frame for the walker)
  // - caller_fp: "managed" frame with a stackmap entry keyed by `return_address`
  let mut mem = AlignedStack([0usize; 64]);
  let base = mem.0.as_mut_ptr() as usize;
  let hi = base + mem.0.len() * core::mem::size_of::<usize>();

  let callee_fp = base + 8 * core::mem::size_of::<usize>();
  let caller_fp = base + 24 * core::mem::size_of::<usize>();
  let return_address = 0x1234usize;

  let caller_sp = callee_fp + 16;
  let base_slot_addr = caller_sp as *mut usize;
  let derived_slot_addr = (caller_sp + 8) as *mut usize;

  unsafe {
    // Frame records.
    (callee_fp as *mut usize).write(caller_fp);
    (callee_fp as *mut usize).add(1).write(return_address);

    (caller_fp as *mut usize).write(0);
    (caller_fp as *mut usize).add(1).write(0);

    // Dummy pointer values.
    base_slot_addr.write(0xAAA0);
    derived_slot_addr.write(0xAAA8);
  }

  // Intentionally lie about stack_size: the old pair-walker implementation used stack_size to
  // reconstruct SP and would underflow/out-of-bounds here.
  let stackmaps = StackMaps::parse(&minimal_statepoint_stackmap(return_address as u32, 0x1000)).unwrap();

  let bounds = StackBounds::new(base as u64, hi as u64).unwrap();
  let mut seen: Vec<(usize, usize)> = Vec::new();

  unsafe {
    runtime_native::stackwalk_fp::walk_gc_root_pairs_from_fp(
      callee_fp as u64,
      Some(bounds),
      &stackmaps,
      |ra, pairs| {
        assert_eq!(ra as usize, return_address);
        for &(base_slot, derived_slot) in pairs {
          seen.push((base_slot as usize, derived_slot as usize));
        }
      },
    )
    .unwrap();
  }

  assert_eq!(seen, vec![(base_slot_addr as usize, derived_slot_addr as usize)]);
}

#[test]
fn root_pairs_skip_non_statepoint_records() {
  let mut mem = AlignedStack([0usize; 64]);
  let base = mem.0.as_mut_ptr() as usize;
  let hi = base + mem.0.len() * core::mem::size_of::<usize>();

  let callee_fp = base + 8 * core::mem::size_of::<usize>();
  let caller_fp = base + 24 * core::mem::size_of::<usize>();
  let return_address = 0x1234usize;

  unsafe {
    // Frame records.
    (callee_fp as *mut usize).write(caller_fp);
    (callee_fp as *mut usize).add(1).write(return_address);
    (caller_fp as *mut usize).write(0);
    (caller_fp as *mut usize).add(1).write(0);
  }

  let stackmaps = StackMaps::parse(&minimal_non_statepoint_stackmap(return_address as u32, 0x1000)).unwrap();
  let bounds = StackBounds::new(base as u64, hi as u64).unwrap();

  let mut called = false;
  unsafe {
    runtime_native::stackwalk_fp::walk_gc_root_pairs_from_fp(
      callee_fp as u64,
      Some(bounds),
      &stackmaps,
      |_ra, _pairs| called = true,
    )
    .expect("walk should succeed");
  }
  assert!(!called, "expected non-statepoint record to be skipped");
}

#[test]
fn root_pairs_reject_misaligned_root_slot() {
  let mut mem = AlignedStack([0usize; 64]);
  let base = mem.0.as_mut_ptr() as usize;
  let hi = base + mem.0.len() * core::mem::size_of::<usize>();

  let callee_fp = base + 8 * core::mem::size_of::<usize>();
  let caller_fp = base + 24 * core::mem::size_of::<usize>();
  let return_address = 0x1234usize;
  let caller_sp = callee_fp + 16;
  let misaligned_slot = (caller_sp as u64) + 1;

  unsafe {
    (callee_fp as *mut usize).write(caller_fp);
    (callee_fp as *mut usize).add(1).write(return_address);
    (caller_fp as *mut usize).write(0);
    (caller_fp as *mut usize).add(1).write(0);
  }

  let stackmaps =
    StackMaps::parse(&minimal_statepoint_stackmap_with_offsets(return_address as u32, 0x1000, 1, 9)).unwrap();
  let bounds = StackBounds::new(base as u64, hi as u64).unwrap();
  let res = unsafe {
    runtime_native::stackwalk_fp::walk_gc_root_pairs_from_fp(
      callee_fp as u64,
      Some(bounds),
      &stackmaps,
      |_ra, _pairs| {},
    )
  };
  assert!(matches!(
    res,
    Err(WalkError::MisalignedRootSlot { slot_addr, .. }) if slot_addr == misaligned_slot
  ));
}

#[test]
fn root_pairs_reject_out_of_bounds_root_slot() {
  let mut mem = AlignedStack([0usize; 64]);
  let base = mem.0.as_mut_ptr() as usize;
  let hi = base + mem.0.len() * core::mem::size_of::<usize>();

  let callee_fp = base + 8 * core::mem::size_of::<usize>();
  let caller_fp = base + 24 * core::mem::size_of::<usize>();
  let return_address = 0x1234usize;
  let caller_sp = callee_fp + 16;
  let oob_off: i32 = 0x1000;
  let expected_slot = (caller_sp as u64) + (oob_off as u64);

  unsafe {
    (callee_fp as *mut usize).write(caller_fp);
    (callee_fp as *mut usize).add(1).write(return_address);
    (caller_fp as *mut usize).write(0);
    (caller_fp as *mut usize).add(1).write(0);
  }

  // Use a large stack_size so the verifier doesn't reject the SP-relative offset; we want the
  // runtime walker to surface the out-of-bounds condition via `StackBounds`.
  let stackmaps = StackMaps::parse(&minimal_statepoint_stackmap_with_offsets(
    return_address as u32,
    0x2000,
    oob_off,
    oob_off + 8,
  ))
  .unwrap();
  let bounds = StackBounds::new(base as u64, hi as u64).unwrap();
  let res = unsafe {
    runtime_native::stackwalk_fp::walk_gc_root_pairs_from_fp(
      callee_fp as u64,
      Some(bounds),
      &stackmaps,
      |_ra, _pairs| {},
    )
  };
  assert!(matches!(
    res,
    Err(WalkError::RootSlotOutOfBounds { slot_addr, .. }) if slot_addr == expected_slot
  ));
}

#[cfg(target_arch = "x86_64")]
#[test]
fn fixture_stack_enumerates_root_pairs_from_stackmaps_with_callsite_sp_adjustment() {
  use runtime_native::stackmaps::Location;
  use runtime_native::statepoints::StatepointRecord;
  use std::collections::BTreeSet;

  let stackmaps =
    StackMaps::parse(include_bytes!("fixtures/bin/statepoint_x86_64.bin")).expect("parse stackmaps");

  // Pick two callsite records so we can build a multi-frame managed call chain.
  let callsites: Vec<(u64, runtime_native::stackmaps::CallSite<'_>)> =
    stackmaps.iter().take(2).collect();
  assert!(
    callsites.len() >= 2,
    "fixture must contain at least two callsites to test multi-frame walking"
  );

  // Fake stack memory.
  let mut stack = vec![0u8; 4096];
  let base = stack.as_mut_ptr() as usize;

  // Construct a 2-frame managed call chain:
  //   runtime_frame (start_fp) -> caller1_fp -> caller2_fp -> null
  let start_fp = align_up(base + 0x100, 16);
  let caller1_fp = align_up(base + 0x600, 16);
  let caller2_fp = align_up(base + 0xB00, 16);

  // Stackmap locations are based on the *callsite* SP in the caller. With frame pointers enforced,
  // we derive that SP from the callee frame pointer (`callee_fp + 16`).
  let caller1_sp_callsite = start_fp + 16;
  let caller2_sp_callsite = caller1_fp + 16;

  unsafe {
    // runtime frame -> caller1
    write_u64(start_fp + 0, caller1_fp as u64);
    write_u64(start_fp + 8, callsites[0].0);

    // caller1 -> caller2
    write_u64(caller1_fp + 0, caller2_fp as u64);
    write_u64(caller1_fp + 8, callsites[1].0);

    // caller2 -> null
    write_u64(caller2_fp + 0, 0);
    write_u64(caller2_fp + 8, 0);
  }

  // Regression guard: ensure our synthetic frame pointers model a callsite with extra SP adjustment
  // (e.g. outgoing stack args) such that reconstructing SP from `stack_size` would be wrong for
  // non-top frames.
  let StackSize::Known(stack_size) = callsites[1].1.stack_size else {
    panic!("fixture must not use StackSize::Unknown for this regression");
  };
  let old_locals = stack_size.checked_sub(8).expect("stack_size < FP_RECORD_SIZE");
  let old_sp = (caller2_fp as u64)
    .checked_sub(old_locals)
    .expect("old SP estimate underflow");
  assert_ne!(
    old_sp, caller2_sp_callsite as u64,
    "test requires callsite SP to differ from stack_size-based estimate"
  );

  // Fill each unique root slot in each frame with a distinct pointer value, and record the
  // expected `(base_slot, derived_slot)` addresses.
  let mut expected_pairs: BTreeSet<(usize, usize)> = BTreeSet::new();
  let mut unique_slots: BTreeSet<usize> = BTreeSet::new();
  for (frame_sp, callsite) in [
    (caller1_sp_callsite, callsites[0].1),
    (caller2_sp_callsite, callsites[1].1),
  ] {
    let statepoint = StatepointRecord::new(callsite.record).expect("decode statepoint layout");
    for pair in statepoint.gc_pairs() {
      let base_slot = match &pair.base {
        Location::Indirect { dwarf_reg, offset, .. } => {
          assert_eq!(*dwarf_reg, 7, "fixture roots must be [SP + off]");
          add_signed_u64(frame_sp as u64, *offset).expect("base slot addr") as usize
        }
        other => panic!("unexpected base location kind in fixture: {other:?}"),
      };
      let derived_slot = match &pair.derived {
        Location::Indirect { dwarf_reg, offset, .. } => {
          assert_eq!(*dwarf_reg, 7, "fixture roots must be [SP + off]");
          add_signed_u64(frame_sp as u64, *offset).expect("derived slot addr") as usize
        }
        other => panic!("unexpected derived location kind in fixture: {other:?}"),
      };
      expected_pairs.insert((base_slot, derived_slot));
      unique_slots.insert(base_slot);
      unique_slots.insert(derived_slot);
    }
  }

  for slot_addr in unique_slots {
    let obj = Box::into_raw(Box::new(0u8)) as u64;
    unsafe {
      write_u64(slot_addr, obj);
    }
  }

  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();
  let mut visited_pairs: BTreeSet<(usize, usize)> = BTreeSet::new();
  unsafe {
    runtime_native::stackwalk_fp::walk_gc_root_pairs_from_fp(
      start_fp as u64,
      Some(bounds),
      &stackmaps,
      |_ra, pairs| {
        for &(base_slot, derived_slot) in pairs {
          visited_pairs.insert((base_slot as usize, derived_slot as usize));
        }
      },
    )
    .expect("walk");
  }

  assert_eq!(visited_pairs, expected_pairs);
}

#[test]
fn root_pairs_from_safepoint_context_use_ctx_sp() {
  let mut mem = AlignedStack([0usize; 64]);
  let base = mem.0.as_mut_ptr() as usize;
  let hi = base + mem.0.len() * core::mem::size_of::<usize>();

  let callee_fp = base + 8 * core::mem::size_of::<usize>();
  let caller_fp = base + 24 * core::mem::size_of::<usize>();
  let return_address = 0x1234usize;

  let caller_sp = callee_fp + 16;
  let base_slot_addr = caller_sp as *mut usize;
  let derived_slot_addr = (caller_sp + 8) as *mut usize;

  unsafe {
    (callee_fp as *mut usize).write(caller_fp);
    (callee_fp as *mut usize).add(1).write(return_address);

    (caller_fp as *mut usize).write(0);
    (caller_fp as *mut usize).add(1).write(0);

    base_slot_addr.write(0xAAA0);
    derived_slot_addr.write(0xAAA8);
  }

  let stackmaps = StackMaps::parse(&minimal_statepoint_stackmap(return_address as u32, 0x1000)).unwrap();
  let bounds = StackBounds::new(base as u64, hi as u64).unwrap();

  #[cfg(target_arch = "x86_64")]
  let sp_entry = caller_sp - arch::WORD_SIZE;
  #[cfg(target_arch = "aarch64")]
  let sp_entry = caller_sp;

  let ctx = arch::SafepointContext {
    sp_entry,
    sp: caller_sp,
    fp: caller_fp,
    ip: return_address,
    regs: core::ptr::null_mut(),
  };

  let mut seen: Vec<(usize, usize)> = Vec::new();
  unsafe {
    runtime_native::stackwalk_fp::walk_gc_root_pairs_from_safepoint_context(
      &ctx,
      Some(bounds),
      &stackmaps,
      |ra, pairs| {
        assert_eq!(ra as usize, return_address);
        for &(base_slot, derived_slot) in pairs {
          seen.push((base_slot as usize, derived_slot as usize));
        }
      },
    )
    .unwrap();
  }

  assert_eq!(seen, vec![(base_slot_addr as usize, derived_slot_addr as usize)]);
}

#[cfg(target_arch = "x86_64")]
fn align_up(v: usize, align: usize) -> usize {
  (v + (align - 1)) & !(align - 1)
}

#[cfg(target_arch = "x86_64")]
unsafe fn write_u64(addr: usize, val: u64) {
  (addr as *mut u64).write_unaligned(val);
}

#[cfg(target_arch = "x86_64")]
fn add_signed_u64(base: u64, offset: i32) -> Option<u64> {
  if offset >= 0 {
    base.checked_add(offset as u64)
  } else {
    base.checked_sub((-offset) as u64)
  }
}
