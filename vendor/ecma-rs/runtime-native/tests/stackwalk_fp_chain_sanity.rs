use runtime_native::{walk_gc_roots_from_fp, StackMaps, WalkError};
use runtime_native::stackmaps::StackSize;
use runtime_native::stackwalk::StackBounds;

#[repr(align(16))]
struct AlignedStack<const N: usize>([u8; N]);

#[cfg(target_arch = "x86_64")]
const DWARF_SP: u16 = 7;
#[cfg(target_arch = "aarch64")]
const DWARF_SP: u16 = 31;

#[cfg(target_arch = "x86_64")]
const FP_RECORD_SIZE: u64 = 8;
#[cfg(target_arch = "aarch64")]
const FP_RECORD_SIZE: u64 = 16;

#[cfg(target_arch = "x86_64")]
const STACK_SIZE: u64 = FP_RECORD_SIZE + 16;
#[cfg(target_arch = "aarch64")]
const STACK_SIZE: u64 = FP_RECORD_SIZE + 16;

#[test]
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn valid_chain_scans_one_frame() {
  let stackmaps = StackMaps::parse(&minimal_stackmaps_blob(DWARF_SP, STACK_SIZE, 0, 0)).unwrap();
  let (callsite_ra, callsite) = stackmaps.iter().next().expect("callsite");
  assert_eq!(callsite.stack_size, StackSize::Known(STACK_SIZE));

  // Synthetic stack buffer (addresses increase upward; stack grows downward).
  let mut mem = AlignedStack([0u8; 512]);
  let base = mem.0.as_mut_ptr() as usize;
  let hi = base + mem.0.len();
  let bounds = StackBounds::new(base as u64, hi as u64).unwrap();

  let locals_size = (STACK_SIZE - FP_RECORD_SIZE) as usize;
  let start_fp = align_up(base + 128, 16);
  // Stackmap SP base at the return address is derived from the *callee* FP:
  // `caller_sp = callee_fp + 16`.
  let caller_sp = start_fp + 16;
  let caller_fp = caller_sp + locals_size;
  assert_eq!(caller_fp % 16, 0);
  assert_eq!(start_fp % 16, 0);

  unsafe {
    // runtime frame -> managed caller frame
    write_u64(start_fp + 0, caller_fp as u64);
    write_u64(start_fp + 8, callsite_ra);

    // managed caller frame -> null (terminates chain)
    write_u64(caller_fp + 0, 0);
    write_u64(caller_fp + 8, 0);

    // Fill the single root slot [SP + 0].
    write_u64(caller_sp + 0, 0x1111);
  }

  let mut visited = Vec::<usize>::new();
  unsafe {
    walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |slot| {
      visited.push(slot as usize);
    })
    .unwrap();
  }

  assert_eq!(visited, vec![caller_sp]);
}

#[test]
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn corrupted_chain_next_fp_lt_fp_returns_error() {
  let stackmaps = StackMaps::parse(&minimal_stackmaps_blob(DWARF_SP, STACK_SIZE, 0, 0)).unwrap();
  let (callsite_ra, _) = stackmaps.iter().next().expect("callsite");

  let mut mem = AlignedStack([0u8; 256]);
  let base = mem.0.as_mut_ptr() as usize;
  let hi = base + mem.0.len();
  let bounds = StackBounds::new(base as u64, hi as u64).unwrap();

  let start_fp = base + 0x80;
  let caller_fp = start_fp - 0x10;
  assert_eq!(start_fp % 16, 0);
  assert_eq!(caller_fp % 16, 0);

  unsafe {
    write_u64(start_fp + 0, caller_fp as u64);
    write_u64(start_fp + 8, callsite_ra);
  }

  let err = unsafe { walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |_slot| {}) }
    .unwrap_err();
  assert!(matches!(err, WalkError::NonMonotonicFp { .. }));
}

#[test]
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn corrupted_chain_next_fp_eq_fp_returns_error() {
  let stackmaps = StackMaps::parse(&minimal_stackmaps_blob(DWARF_SP, STACK_SIZE, 0, 0)).unwrap();
  let (callsite_ra, _) = stackmaps.iter().next().expect("callsite");

  let mut mem = AlignedStack([0u8; 256]);
  let base = mem.0.as_mut_ptr() as usize;
  let hi = base + mem.0.len();
  let bounds = StackBounds::new(base as u64, hi as u64).unwrap();

  let start_fp = base + 0x80;
  assert_eq!(start_fp % 16, 0);

  unsafe {
    // Self-loop.
    write_u64(start_fp + 0, start_fp as u64);
    write_u64(start_fp + 8, callsite_ra);
  }

  let err = unsafe { walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |_slot| {}) }
    .unwrap_err();
  assert!(matches!(err, WalkError::NonMonotonicFp { .. }));
}

#[test]
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn out_of_bounds_start_fp_returns_error() {
  let stackmaps = StackMaps::parse(&minimal_stackmaps_blob(DWARF_SP, STACK_SIZE, 0, 0)).unwrap();

  let mem = AlignedStack([0u8; 256]);
  let base = mem.0.as_ptr() as usize;
  let hi = base + mem.0.len();
  let bounds = StackBounds::new(base as u64, hi as u64).unwrap();

  // Aligned but outside the bounds.
  let start_fp = hi;
  assert_eq!(start_fp % 16, 0);

  let err = unsafe { walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |_slot| {}) }
    .unwrap_err();
  assert!(matches!(err, WalkError::FramePointerOutOfBounds { .. }));
}

fn align_up(v: usize, align: usize) -> usize {
  (v + (align - 1)) & !(align - 1)
}

unsafe fn write_u64(addr: usize, val: u64) {
  (addr as *mut u64).write_unaligned(val);
}

fn minimal_stackmaps_blob(dwarf_sp: u16, stack_size: u64, base_off: i32, derived_off: i32) -> Vec<u8> {
  // Minimal StackMap v3 blob containing one callsite record. The record uses a
  // statepoint-like layout:
  // - 3 constant header locations, followed by
  // - one (base, derived) Indirect pair at [SP + base_off] / [SP + derived_off].
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
  out.extend_from_slice(&stack_size.to_le_bytes()); // stack_size
  out.extend_from_slice(&1u64.to_le_bytes()); // record_count

  // One record.
  out.extend_from_slice(&0xabcdef00u64.to_le_bytes()); // patchpoint_id
  out.extend_from_slice(&0x10u32.to_le_bytes()); // instruction_offset
  out.extend_from_slice(&0u16.to_le_bytes()); // reserved
  out.extend_from_slice(&5u16.to_le_bytes()); // num_locations (3 header consts + 2 GC locs)

  // Helper: location entry (12 bytes).
  fn push_loc(out: &mut Vec<u8>, kind: u8, size: u16, dwarf_reg: u16, offset: i32) {
    out.push(kind);
    out.push(0); // reserved0
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&dwarf_reg.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved1
    out.extend_from_slice(&offset.to_le_bytes());
  }

  // 3 constant header locations.
  for _ in 0..3 {
    push_loc(&mut out, 4, 8, 0, 0);
  }

  // base + derived locations.
  push_loc(&mut out, 3, 8, dwarf_sp, base_off);
  push_loc(&mut out, 3, 8, dwarf_sp, derived_off);

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
