use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use runtime_native::stackwalk::StackBounds;
use runtime_native::stackwalk_fp::ensure_stackwalk_scratch_capacity;
use runtime_native::{walk_gc_roots_from_fp, StackMaps};

struct CountingAlloc;

static ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    System.alloc(layout)
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    System.alloc_zeroed(layout)
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    System.dealloc(ptr, layout)
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    System.realloc(ptr, layout, new_size)
  }
}

#[test]
fn stackwalk_supports_large_gc_pair_count_without_allocations() {
  const GC_PAIR_COUNT: usize = 2048;
  const FUNCTION_ADDR: u64 = 0x1000;
  const INSTRUCTION_OFFSET: u32 = 0x10;

  let blob = build_stackmap_with_statepoint_gc_pairs(FUNCTION_ADDR, INSTRUCTION_OFFSET, GC_PAIR_COUNT);
  let stackmaps = StackMaps::parse(&blob).expect("parse synthetic stackmaps");
  assert_eq!(stackmaps.max_gc_pairs_per_frame(), GC_PAIR_COUNT);

  // Preallocate the scratch buffers outside the measured section.
  ensure_stackwalk_scratch_capacity(stackmaps.max_gc_pairs_per_frame());

  // Fake stack memory (heap-backed).
  let mut stack = vec![0u8; 0x20_000];
  let base = stack.as_mut_ptr() as usize;

  let lo = base as u64;
  let hi = base.saturating_add(stack.len()) as u64;
  let bounds = StackBounds::new(lo, hi).expect("stack bounds");

  // Two frames:
  //   runtime frame (start_fp) -> managed frame (caller_fp) -> null.
  let start_fp = align_up(base + 0x1000, 16);
  let caller_fp = align_up(base + 0x18000, 16);
  assert!(caller_fp > start_fp);

  let callsite_ra = FUNCTION_ADDR + (INSTRUCTION_OFFSET as u64);

  unsafe {
    write_u64(start_fp + 0, caller_fp as u64);
    write_u64(start_fp + 8, callsite_ra);

    write_u64(caller_fp + 0, 0);
    write_u64(caller_fp + 8, 0);
  }

  let mut visited = 0usize;
  ALLOC_CALLS.store(0, Ordering::SeqCst);

  // Safety: the synthetic frame pointers and stack bounds point into `stack` above.
  unsafe {
    walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |_slot| {
      visited += 1;
    })
    .expect("stack walk should succeed");
  }

  assert_eq!(
    visited, GC_PAIR_COUNT,
    "expected one unique base root slot per gc-live pair"
  );

  let allocs = ALLOC_CALLS.load(Ordering::SeqCst);
  assert_eq!(allocs, 0, "stack root enumeration allocated (alloc calls={allocs})");
}

fn align_up(v: usize, align: usize) -> usize {
  debug_assert!(align.is_power_of_two());
  (v + (align - 1)) & !(align - 1)
}

unsafe fn write_u64(addr: usize, val: u64) {
  (addr as *mut u64).write_unaligned(val);
}

fn build_stackmap_with_statepoint_gc_pairs(
  function_addr: u64,
  instruction_offset: u32,
  gc_pair_count: usize,
) -> Vec<u8> {
  assert!(gc_pair_count <= (u16::MAX as usize - 3) / 2);
  let num_locations = 3 + (gc_pair_count * 2);
  let num_locations_u16 = u16::try_from(num_locations).expect("num_locations fits u16");

  let mut out: Vec<u8> = Vec::new();

  // StackMap v3 header.
  push_u8(&mut out, 3); // version
  push_u8(&mut out, 0); // reserved0
  push_u16(&mut out, 0); // reserved1
  push_u32(&mut out, 1); // num_functions
  push_u32(&mut out, 0); // num_constants
  push_u32(&mut out, 1); // num_records

  // Single function record.
  push_u64(&mut out, function_addr);
  // `stack_size` must be large enough to cover the highest SP-relative indirect location offset,
  // otherwise the statepoint verifier will reject the stackmap.
  //
  // Use a slightly larger size than strictly required so the GC root slots don't overlap the frame
  // record (saved FP / return address) in our synthetic stack layout.
  let stack_size = u64::try_from((gc_pair_count + 2) * 8).expect("stack_size fits u64");
  push_u64(&mut out, stack_size);
  push_u64(&mut out, 1); // record_count

  // Single record (statepoint layout).
  push_u64(&mut out, 1); // patchpoint_id (not used for statepoint detection)
  push_u32(&mut out, instruction_offset);
  push_u16(&mut out, 0); // reserved
  push_u16(&mut out, num_locations_u16);

  // 3 statepoint header constants (callconv, flags, deopt_count=0).
  for _ in 0..3 {
    // Location: Constant
    push_u8(&mut out, 4); // kind = Constant
    push_u8(&mut out, 0); // reserved
    push_u16(&mut out, 8); // size
    push_u16(&mut out, 0); // dwarf_reg
    push_u16(&mut out, 0); // reserved
    push_i32(&mut out, 0); // small const
  }

  // Emit `gc_pair_count` pairs where base==derived and each pair uses a unique stack slot.
  for i in 0..gc_pair_count {
    let off = i32::try_from(i * 8).expect("offset fits i32");
    for _ in 0..2 {
      // Location: Indirect [SP + off]
      push_u8(&mut out, 3); // kind = Indirect
      push_u8(&mut out, 0); // reserved
      push_u16(&mut out, 8); // size
      push_u16(&mut out, runtime_native::stackwalk::DWARF_SP_REG); // dwarf_reg (SP)
      push_u16(&mut out, 0); // reserved
      push_i32(&mut out, off);
    }
  }

  // StackMap v3 aligns the live-out header to 8 bytes after the locations array.
  align_to_8_with(&mut out, 0);
  // No live-outs.
  push_u16(&mut out, 0);
  push_u16(&mut out, 0);
  align_to_8_with(&mut out, 0);

  out
}

fn align_to_8_with(buf: &mut Vec<u8>, fill: u8) {
  while buf.len() % 8 != 0 {
    buf.push(fill);
  }
}

fn push_u8(buf: &mut Vec<u8>, v: u8) {
  buf.push(v);
}

fn push_u16(buf: &mut Vec<u8>, v: u16) {
  buf.extend_from_slice(&v.to_le_bytes());
}

fn push_u32(buf: &mut Vec<u8>, v: u32) {
  buf.extend_from_slice(&v.to_le_bytes());
}

fn push_u64(buf: &mut Vec<u8>, v: u64) {
  buf.extend_from_slice(&v.to_le_bytes());
}

fn push_i32(buf: &mut Vec<u8>, v: i32) {
  buf.extend_from_slice(&v.to_le_bytes());
}
