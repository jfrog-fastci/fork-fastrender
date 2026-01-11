#![cfg(all(target_arch = "x86_64", target_os = "linux"))]

use object::{Object, ObjectSection};
use runtime_native::gc::{ObjHeader, RootSet, TypeDescriptor};
use runtime_native::stackmaps::Location;
use runtime_native::statepoints::StatepointRecord;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{gc::SimpleRememberedSet, GcHeap, StackMaps};
use std::mem;
use std::ptr;

const FIXTURE_OBJ: &[u8] =
  include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/statepoint_fixture.o"));

fn fixture_stackmap_sp_offsets() -> Vec<i32> {
  let obj = object::File::parse(FIXTURE_OBJ).expect("failed to parse statepoint_fixture.o");
  let section = obj
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section in fixture object");
  let stackmap_bytes = section
    .data()
    .expect("failed to read .llvm_stackmaps section bytes");

  let stackmaps = StackMaps::parse(stackmap_bytes).expect("failed to parse .llvm_stackmaps section");
  let (_callsite_ra, callsite) = stackmaps.iter().next().expect("fixture stackmaps empty");
  let statepoint = StatepointRecord::new(callsite.record).expect("failed to decode statepoint record");

  let mut offsets = Vec::with_capacity(statepoint.gc_pairs().len() * 2);
  for pair in statepoint.gc_pairs() {
    for loc in [pair.base, pair.derived] {
      match loc {
        Location::Indirect {
          dwarf_reg: 7, // x86_64 DWARF register number for RSP
          offset,
          size: 8,
        } => offsets.push(*offset),
        other => panic!("unexpected gc-live location in fixture stackmaps: {other:?}"),
      }
    }
  }

  offsets.sort_unstable();
  offsets.dedup();
  offsets
}

fn add_signed(base: usize, offset: i32) -> Option<usize> {
  if offset >= 0 {
    base.checked_add(offset as usize)
  } else {
    base.checked_sub((-offset) as usize)
  }
}

fn slot_ptr(frame_start: usize, frame_end: usize, sp_base: usize, offset: i32) -> *mut *mut u8 {
  let addr = add_signed(sp_base, offset).expect("slot address overflow");
  assert!(
    addr >= frame_start && addr + mem::size_of::<*mut u8>() <= frame_end,
    "slot address out of bounds: frame=[{frame_start:#x},{frame_end:#x}) sp_base={sp_base:#x} offset={offset} addr={addr:#x}"
  );
  assert_eq!(
    addr % mem::align_of::<*mut u8>(),
    0,
    "slot address is not pointer-aligned: addr={addr:#x}"
  );
  addr as *mut *mut u8
}

struct StackMapRoots {
  slots: Vec<*mut *mut u8>,
}

impl RootSet for StackMapRoots {
  fn for_each_root_slot(&mut self, f: &mut dyn FnMut(*mut *mut u8)) {
    for &slot in &self.slots {
      f(slot);
    }
  }
}

#[repr(C)]
struct Blob {
  header: ObjHeader,
  a: u64,
  b: u64,
}

static BLOB_DESC: TypeDescriptor = TypeDescriptor::new(mem::size_of::<Blob>(), &[]);

#[test]
fn statepoint_fixture_stackmaps_drive_minor_gc_root_updates() {
  let _rt = TestRuntimeGuard::new();

  let offsets = fixture_stackmap_sp_offsets();
  assert!(
    offsets.len() >= 2,
    "fixture must contain at least 2 unique gc-live stack slots, got {offsets:?}"
  );
  assert!(
    offsets.iter().any(|&o| o != 0),
    "fixture must include a non-zero stack slot offset, got {offsets:?}"
  );

  // Fake "stack frame" memory. Use `u64` words so pointer slots are naturally aligned.
  let mut frame = vec![0u64; 1024]; // 8 KiB
  let frame_start = frame.as_mut_ptr() as usize;
  let frame_end = frame_start + frame.len() * mem::size_of::<u64>();
  let sp_base = frame_start + 4096;

  let slots: Vec<*mut *mut u8> = offsets
    .iter()
    .copied()
    .map(|off| slot_ptr(frame_start, frame_end, sp_base, off))
    .collect();

  // Initialize all root slots to null to ensure we don't accidentally keep other objects alive.
  unsafe {
    for &slot in &slots {
      slot.write(ptr::null_mut());
    }
  }

  // Pick one non-zero offset for the moving nursery object (exercise offset arithmetic).
  let moving_off = offsets
    .iter()
    .copied()
    .find(|&o| o != 0)
    .expect("non-empty offsets (checked above)");
  let stable_off = offsets
    .iter()
    .copied()
    .find(|&o| o != moving_off)
    .expect("need at least two offsets (checked above)");

  let moving_slot = slot_ptr(frame_start, frame_end, sp_base, moving_off);
  let stable_slot = slot_ptr(frame_start, frame_end, sp_base, stable_off);

  let mut heap = GcHeap::new();

  let young = heap.alloc_young(&BLOB_DESC);
  unsafe {
    let y = &mut *(young as *mut Blob);
    y.a = 0x1111_1111_1111_1111;
    y.b = 0x2222_2222_2222_2222;
  }

  // A non-moving root (LOS pinned allocation).
  let pinned = heap.alloc_pinned(&BLOB_DESC);
  unsafe {
    let p = &mut *(pinned as *mut Blob);
    p.a = 0xaaaa_aaaa_aaaa_aaaa;
    p.b = 0xbbbb_bbbb_bbbb_bbbb;
  }

  unsafe {
    moving_slot.write(young);
    stable_slot.write(pinned);
  }

  let mut roots = StackMapRoots { slots };
  let mut remembered = SimpleRememberedSet::new();
  heap.collect_minor(&mut roots, &mut remembered);

  let moved = unsafe { moving_slot.read() };
  assert_ne!(moved, young, "minor GC should evacuate nursery object and update slot");
  assert!(
    !heap.is_in_nursery(moved),
    "updated slot must not point into the nursery after evacuation"
  );
  assert!(heap.is_in_immix(moved), "evacuated object should be in Immix");
  unsafe {
    let y = &*(moved as *const Blob);
    assert_eq!(y.a, 0x1111_1111_1111_1111);
    assert_eq!(y.b, 0x2222_2222_2222_2222);
  }

  let pinned_after = unsafe { stable_slot.read() };
  assert_eq!(pinned_after, pinned, "pinned LOS object must not move during minor GC");
  assert!(heap.is_in_los(pinned_after));
  unsafe {
    let p = &*(pinned_after as *const Blob);
    assert_eq!(p.a, 0xaaaa_aaaa_aaaa_aaaa);
    assert_eq!(p.b, 0xbbbb_bbbb_bbbb_bbbb);
  }
}

#[test]
fn perturbed_offsets_do_not_update_the_real_root_slots() {
  let _rt = TestRuntimeGuard::new();

  let offsets = fixture_stackmap_sp_offsets();
  assert!(offsets.len() >= 2);

  // Choose a delta that stays aligned and doesn't collide with the real offsets.
  let delta: i32 = [8, 16, 32, 64]
    .into_iter()
    .find(|&d| offsets.iter().all(|&off| !offsets.contains(&(off + d))))
    .expect("unable to find non-colliding delta for perturbed offsets");

  let mut frame = vec![0u64; 1024];
  let frame_start = frame.as_mut_ptr() as usize;
  let frame_end = frame_start + frame.len() * mem::size_of::<u64>();
  let sp_base = frame_start + 4096;

  let real_slots: Vec<*mut *mut u8> = offsets
    .iter()
    .copied()
    .map(|off| slot_ptr(frame_start, frame_end, sp_base, off))
    .collect();

  unsafe {
    for &slot in &real_slots {
      slot.write(ptr::null_mut());
    }
  }

  let moving_off = offsets
    .iter()
    .copied()
    .find(|&o| o != 0)
    .unwrap_or(offsets[0]);
  let moving_slot = slot_ptr(frame_start, frame_end, sp_base, moving_off);

  let mut heap = GcHeap::new();
  let young = heap.alloc_young(&BLOB_DESC);
  unsafe {
    (*(young as *mut Blob)).a = 0x1234_5678_9abc_def0;
    (*(young as *mut Blob)).b = 0x0fed_cba9_8765_4321;
    moving_slot.write(young);
  }

  // Enumerate the wrong (perturbed) slots: GC should not see `young` as a root.
  let wrong_slots: Vec<*mut *mut u8> = offsets
    .iter()
    .copied()
    .map(|off| slot_ptr(frame_start, frame_end, sp_base, off + delta))
    .collect();
  let mut roots = StackMapRoots { slots: wrong_slots };
  let mut remembered = SimpleRememberedSet::new();
  heap.collect_minor(&mut roots, &mut remembered);

  let slot_after = unsafe { moving_slot.read() };
  assert_eq!(
    slot_after, young,
    "wrong offsets must not update the real root slot"
  );
  assert!(
    heap.is_in_nursery(slot_after),
    "without being enumerated as a root, the pointer should remain a nursery address"
  );
}

