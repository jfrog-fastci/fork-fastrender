#![cfg(all(target_arch = "x86_64", target_os = "linux"))]

use object::{Object, ObjectSection};
use runtime_native::gc::{ObjHeader, RootSet, TypeDescriptor};
use runtime_native::stackmaps::Location;
use runtime_native::statepoints::StatepointRecord;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{gc::SimpleRememberedSet, GcHeap, StackMaps};
use std::fs;
use std::mem;
use std::path::Path;
use std::ptr;
use std::process::{Command, Stdio};

const FIXTURE_OBJ: &[u8] =
  include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/statepoint_fixture.o"));

/// Expected SP-relative offsets for `"gc-live"` stack slots in the checked-in
/// `statepoint_fixture.o`.
///
/// These offsets were inspected via:
///   `llvm-readobj-18 --stackmap statepoint_fixture.o`
///
/// They are used as an independent ground truth so the test fails if the
/// stackmap parsing / location decoding logic returns the wrong offsets.
const EXPECTED_SP_OFFSETS: &[i32] = &[8, 16];

fn stackmap_sp_offsets_from_obj(obj_bytes: &[u8]) -> Vec<i32> {
  let obj = object::File::parse(obj_bytes).expect("failed to parse stackmap fixture object");
  let section = obj
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section in fixture object");
  let stackmap_bytes = section
    .data()
    .expect("failed to read .llvm_stackmaps section bytes");

  let stackmaps = StackMaps::parse(stackmap_bytes).expect("failed to parse .llvm_stackmaps section");
  let (_callsite_ra, callsite) = stackmaps.iter().next().expect("fixture stackmaps empty");
  let statepoint = StatepointRecord::new(callsite.record).expect("failed to decode statepoint record");

  let mut offsets = Vec::with_capacity(statepoint.gc_pair_count() * 2);
  for pair in statepoint.gc_pairs() {
    for loc in [&pair.base, &pair.derived] {
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

fn fixture_stackmap_sp_offsets() -> Vec<i32> {
  stackmap_sp_offsets_from_obj(FIXTURE_OBJ)
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

  let parsed_offsets = fixture_stackmap_sp_offsets();
  assert_eq!(
    parsed_offsets,
    EXPECTED_SP_OFFSETS,
    "fixture stackmap offsets changed; if the fixture was regenerated, update EXPECTED_SP_OFFSETS"
  );

  // Fake "stack frame" memory. Use `u64` words so pointer slots are naturally aligned.
  let mut frame = vec![0u64; 1024]; // 8 KiB
  let frame_start = frame.as_mut_ptr() as usize;
  let frame_end = frame_start + frame.len() * mem::size_of::<u64>();
  let sp_base = frame_start + 4096;

  let slots: Vec<*mut *mut u8> = parsed_offsets
    .iter()
    .copied()
    .map(|off| slot_ptr(frame_start, frame_end, sp_base, off))
    .collect();

  // Independently use the expected offsets as the "real" stack slots that must
  // be updated. If parsing yields wrong offsets, the GC will enumerate the wrong
  // slots and these will not change, failing the assertions below.
  let moving_slot = slot_ptr(frame_start, frame_end, sp_base, EXPECTED_SP_OFFSETS[1]);
  let stable_slot = slot_ptr(frame_start, frame_end, sp_base, EXPECTED_SP_OFFSETS[0]);

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
  heap.collect_minor(&mut roots, &mut remembered).unwrap();

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

  // Choose a delta that stays aligned and doesn't collide with the real offsets.
  let delta: i32 = [8, 16, 32, 64]
    .into_iter()
    .find(|&d| EXPECTED_SP_OFFSETS.iter().all(|&off| !EXPECTED_SP_OFFSETS.contains(&(off + d))))
    .expect("unable to find non-colliding delta for perturbed offsets");

  let mut frame = vec![0u64; 1024];
  let frame_start = frame.as_mut_ptr() as usize;
  let frame_end = frame_start + frame.len() * mem::size_of::<u64>();
  let sp_base = frame_start + 4096;

  let real_slots: Vec<*mut *mut u8> = EXPECTED_SP_OFFSETS
    .iter()
    .copied()
    .map(|off| slot_ptr(frame_start, frame_end, sp_base, off))
    .collect();

  unsafe {
    for &slot in &real_slots {
      slot.write(ptr::null_mut());
    }
  }

  let moving_slot = slot_ptr(frame_start, frame_end, sp_base, EXPECTED_SP_OFFSETS[1]);

  let mut heap = GcHeap::new();
  let young = heap.alloc_young(&BLOB_DESC);
  unsafe {
    (*(young as *mut Blob)).a = 0x1234_5678_9abc_def0;
    (*(young as *mut Blob)).b = 0x0fed_cba9_8765_4321;
    moving_slot.write(young);
  }

  // Enumerate the wrong (perturbed) slots: GC should not see `young` as a root.
  let wrong_slots: Vec<*mut *mut u8> = EXPECTED_SP_OFFSETS
    .iter()
    .copied()
    .map(|off| slot_ptr(frame_start, frame_end, sp_base, off + delta))
    .collect();
  let mut roots = StackMapRoots { slots: wrong_slots };
  let mut remembered = SimpleRememberedSet::new();
  heap.collect_minor(&mut roots, &mut remembered).unwrap();

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

fn find_llc_18() -> Option<String> {
  for cand in ["llc-18", "llc"] {
    let out = Command::new(cand)
      .arg("--version")
      .stdout(Stdio::piped())
      .stderr(Stdio::null())
      .output()
      .ok()?;
    if !out.status.success() {
      continue;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.contains("LLVM version 18") {
      return Some(cand.to_string());
    }
  }
  None
}

#[test]
#[ignore]
fn llvm18_can_regenerate_fixture_and_offsets_match() {
  let Some(llc) = find_llc_18() else {
    eprintln!("skipping: unable to locate LLVM 18 llc (`llc-18` or `llc` w/ version 18)");
    return;
  };

  let td = tempfile::tempdir().expect("create tempdir");
  let out_obj = td.path().join("statepoint_fixture.o");
  let ll_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/statepoint_fixture.ll");

  let status = Command::new(&llc)
    .args(["-O0", "-filetype=obj", "-o"])
    .arg(&out_obj)
    .arg(&ll_path)
    .status()
    .expect("spawn llc");
  assert!(status.success(), "{llc} failed with status {status}");

  let obj_bytes = fs::read(&out_obj).expect("read regenerated object");
  let offsets = stackmap_sp_offsets_from_obj(&obj_bytes);
  assert_eq!(
    offsets,
    EXPECTED_SP_OFFSETS,
    "LLVM-generated stackmap offsets differed; update the checked-in fixture and EXPECTED_SP_OFFSETS if LLVM changes"
  );
}
