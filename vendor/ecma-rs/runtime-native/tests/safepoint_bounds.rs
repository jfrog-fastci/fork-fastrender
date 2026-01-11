#![cfg(all(
  any(target_os = "linux", target_os = "macos"),
  target_pointer_width = "64",
  any(target_arch = "x86_64", target_arch = "aarch64"),
))]

use runtime_native::safepoint::visit_reloc_pairs_with_bounds;
use runtime_native::stackwalk::StackBounds;
use runtime_native::WalkError;

const CALLSITE_PC: u64 = 0x1010;
const STACKMAP_FUNC_ADDR: u64 = 0x1000;
const STACKMAP_INST_OFF: u32 = 0x10;
const ROOT_OFFSET: i32 = 32;

#[repr(align(8))]
struct AlignedStackMap([u8; 128]);

// Embed a tiny StackMap v3 blob into the current test binary so the public
// safepoint helpers can load it via `stackmaps_section()`.
//
// Note: the function address does not correspond to real code in the test
// process; we use it only as a lookup key.
#[used]
#[cfg_attr(target_os = "linux", link_section = ".data.rel.ro.llvm_stackmaps")]
#[cfg_attr(target_os = "macos", link_section = "__LLVM_STACKMAPS,__llvm_stackmaps")]
static TEST_STACKMAP_BLOB: AlignedStackMap = AlignedStackMap(build_stackmap_blob(
  runtime_native::stackwalk::DWARF_SP_REG,
));

const fn build_stackmap_blob(dwarf_sp: u16) -> [u8; 128] {
  const fn push_u8(buf: &mut [u8; 128], mut off: usize, v: u8) -> usize {
    buf[off] = v;
    off += 1;
    off
  }

  const fn push_u16(buf: &mut [u8; 128], mut off: usize, v: u16) -> usize {
    let b = v.to_le_bytes();
    buf[off] = b[0];
    buf[off + 1] = b[1];
    off += 2;
    off
  }

  const fn push_u32(buf: &mut [u8; 128], mut off: usize, v: u32) -> usize {
    let b = v.to_le_bytes();
    buf[off] = b[0];
    buf[off + 1] = b[1];
    buf[off + 2] = b[2];
    buf[off + 3] = b[3];
    off += 4;
    off
  }

  const fn push_i32(buf: &mut [u8; 128], mut off: usize, v: i32) -> usize {
    let b = v.to_le_bytes();
    buf[off] = b[0];
    buf[off + 1] = b[1];
    buf[off + 2] = b[2];
    buf[off + 3] = b[3];
    off += 4;
    off
  }

  const fn push_u64(buf: &mut [u8; 128], mut off: usize, v: u64) -> usize {
    let b = v.to_le_bytes();
    buf[off] = b[0];
    buf[off + 1] = b[1];
    buf[off + 2] = b[2];
    buf[off + 3] = b[3];
    buf[off + 4] = b[4];
    buf[off + 5] = b[5];
    buf[off + 6] = b[6];
    buf[off + 7] = b[7];
    off += 8;
    off
  }

  const fn align_to(off: usize, align: usize) -> usize {
    (off + (align - 1)) & !(align - 1)
  }

  const fn push_location_constant(buf: &mut [u8; 128], mut off: usize, value: i32) -> usize {
    // kind = Constant
    off = push_u8(buf, off, 4);
    off = push_u8(buf, off, 0);
    off = push_u16(buf, off, 8);
    off = push_u16(buf, off, 0);
    off = push_u16(buf, off, 0);
    off = push_i32(buf, off, value);
    off
  }

  const fn push_location_indirect(
    buf: &mut [u8; 128],
    mut off: usize,
    dwarf_reg: u16,
    offset: i32,
  ) -> usize {
    // kind = Indirect [reg + offset]
    off = push_u8(buf, off, 3);
    off = push_u8(buf, off, 0);
    off = push_u16(buf, off, 8);
    off = push_u16(buf, off, dwarf_reg);
    off = push_u16(buf, off, 0);
    off = push_i32(buf, off, offset);
    off
  }

  let mut buf = [0u8; 128];
  let mut off = 0usize;

  // StackMap v3 header.
  off = push_u8(&mut buf, off, 3); // version
  off = push_u8(&mut buf, off, 0); // reserved
  off = push_u16(&mut buf, off, 0); // reserved
  off = push_u32(&mut buf, off, 1); // num_functions
  off = push_u32(&mut buf, off, 0); // num_constants
  off = push_u32(&mut buf, off, 1); // num_records

  // Single function record with one callsite record.
  off = push_u64(&mut buf, off, STACKMAP_FUNC_ADDR);
  off = push_u64(&mut buf, off, 32); // stack_size (unused by this test)
  off = push_u64(&mut buf, off, 1); // record_count

  // Callsite record.
  off = push_u64(&mut buf, off, 1); // patchpoint_id
  off = push_u32(&mut buf, off, STACKMAP_INST_OFF); // instruction_offset
  off = push_u16(&mut buf, off, 0); // reserved
  off = push_u16(&mut buf, off, 5); // num_locations

  // LLVM statepoint header constants: callconv, flags, deopt_count.
  off = push_location_constant(&mut buf, off, 0);
  off = push_location_constant(&mut buf, off, 0);
  off = push_location_constant(&mut buf, off, 0); // deopt_count=0

  // Single GC root pair: base and derived are the same stack slot.
  off = push_location_indirect(&mut buf, off, dwarf_sp, ROOT_OFFSET);
  off = push_location_indirect(&mut buf, off, dwarf_sp, ROOT_OFFSET);

  // StackMap v3 pads so the live-out header begins on an 8-byte boundary.
  off = align_to(off, 8);
  off = push_u16(&mut buf, off, 0); // padding
  off = push_u16(&mut buf, off, 0); // num_liveouts
  off = align_to(off, 8);

  // `off` should now be 128 (exact buffer length).
  let _ = off;
  buf
}

#[test]
fn visit_reloc_pairs_with_bounds_respects_caller_bounds() {
  assert_eq!(TEST_STACKMAP_BLOB.0[0], 3, "expected StackMap v3 header");

  // Put the synthetic "stack" on the heap; if `visit_reloc_pairs_with_bounds` incorrectly falls
  // back to `StackBounds::current_thread()`, the bounds checks will reject these heap addresses.
  let mut stack = vec![0u8; 4096];
  let base = stack.as_mut_ptr() as usize;

  let runtime_fp = align_up(base + 3072, 16);
  let caller_fp = runtime_fp + 16;
  let caller_sp = runtime_fp + 16;
  let slot_addr = caller_sp as u64 + (ROOT_OFFSET as u64);
  let slot_addr = slot_addr as usize;

  assert_eq!(runtime_fp % 16, 0);
  assert_eq!(caller_fp % 16, 0);
  assert_eq!(slot_addr % 8, 0);

  // Build a two-frame chain: runtime frame -> managed caller frame -> 0.
  unsafe {
    write_u64(runtime_fp + 0, caller_fp as u64);
    write_u64(runtime_fp + 8, CALLSITE_PC);
    write_u64(caller_fp + 0, 0);
    write_u64(caller_fp + 8, 0);

    // Seed the root slot value so we can validate `(slot, value)` pairs.
    (slot_addr as *mut *mut u8).write_unaligned(0xdead_beefusize as *mut u8);
  }

  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();

  let mut visited: Vec<usize> = Vec::new();
  let mut values: Vec<usize> = Vec::new();
  visit_reloc_pairs_with_bounds(runtime_fp as u64, Some(bounds), &mut |slot, value| {
    visited.push(slot as usize);
    values.push(value as usize);
  })
  .expect("walk should succeed with correct bounds");

  visited.sort_unstable();
  assert_eq!(visited, vec![slot_addr]);
  assert_eq!(values, vec![0xdead_beefusize]);

  // Now pass bounds that cover the frame-pointer chain but deliberately exclude the root slot.
  let bad_bounds = StackBounds::new(base as u64, (caller_fp + 16) as u64).unwrap();
  let err =
    visit_reloc_pairs_with_bounds(runtime_fp as u64, Some(bad_bounds), &mut |_slot, _value| {})
      .expect_err("walk should fail with out-of-bounds root slot");
  assert!(
    matches!(err, WalkError::RootSlotOutOfBounds { slot_addr: a, .. } if a as usize == slot_addr),
    "expected RootSlotOutOfBounds for slot_addr={slot_addr:#x}, got {err:?}"
  );
}

fn align_up(v: usize, align: usize) -> usize {
  (v + (align - 1)) & !(align - 1)
}

unsafe fn write_u64(addr: usize, val: u64) {
  (addr as *mut u64).write_unaligned(val);
}
