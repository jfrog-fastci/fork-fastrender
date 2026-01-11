#[cfg(target_arch = "x86_64")]
use runtime_native::{walk_gc_roots_from_fp, StackMaps};

#[cfg(target_arch = "aarch64")]
use runtime_native::{walk_gc_roots_from_fp, StackMaps};

use runtime_native::stackwalk::StackBounds;

#[cfg(target_arch = "x86_64")]
#[test]
fn synthetic_stack_enumerates_roots_from_stackmaps() {
  use runtime_native::stackmaps::Location;
  use runtime_native::statepoints::StatepointRecord;
  use std::collections::BTreeMap;

  let stackmaps = StackMaps::parse(include_bytes!("fixtures/bin/statepoint_x86_64.bin"))
    .expect("parse stackmaps");

  // Pick two callsite records so we can build a multi-frame managed call chain.
  let callsites: Vec<(u64, runtime_native::stackmaps::CallSite<'_>)> =
    stackmaps.iter().take(2).collect();
  assert!(
    callsites.len() >= 2,
    "fixture must contain at least two callsites to test multi-frame walking"
  );

  let stack_size = callsites[0].1.stack_size;
  assert_eq!(
    callsites[1].1.stack_size, stack_size,
    "fixture callsites should share a single function stack_size"
  );

  // Fake stack memory.
  let mut stack = vec![0u8; 2048];
  let base = stack.as_mut_ptr() as usize;

  // We choose SP explicitly and compute FP from it. This lets the test validate
  // the walker's FP→SP reconstruction formula.
  //
  // x86_64 FP_RECORD_SIZE=8.
  let fp_delta = (stack_size - 8) as usize;
  let caller1_sp = align_up(base + 512, 16);
  let caller1_fp = caller1_sp + fp_delta;
  let caller2_sp = align_up(base + 1024, 16);
  let caller2_fp = caller2_sp + fp_delta;

  // Start from a runtime frame that returns to `caller1` at callsite 0.
  let start_fp = align_up(base + 256, 16);

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

  // Fill each unique root slot in each frame with a distinct pointer value, and
  // record the expected slot->value mapping.
  let mut expected: BTreeMap<usize, usize> = BTreeMap::new();
  for (frame_sp, callsite) in [(caller1_sp, callsites[0].1), (caller2_sp, callsites[1].1)] {
    let statepoint = StatepointRecord::new(callsite.record).expect("decode statepoint layout");

    let mut slots: Vec<usize> = Vec::new();
    for pair in statepoint.gc_pairs() {
      for loc in [&pair.base, &pair.derived] {
        match loc {
          Location::Indirect { dwarf_reg, offset, .. } => {
            assert_eq!(*dwarf_reg, 7, "fixture roots must be [SP + off]");
            let slot_addr = add_signed_u64(frame_sp as u64, *offset).expect("slot addr");
            slots.push(slot_addr as usize);
          }
          other => panic!("unexpected root location kind in fixture: {other:?}"),
        }
      }
    }
    slots.sort_unstable();
    slots.dedup();

    for slot_addr in slots {
      let obj = Box::into_raw(Box::new(0u8)) as usize;
      unsafe {
        write_u64(slot_addr, obj as u64);
      }
      expected.insert(slot_addr, obj);
    }
  }

  let mut visited: BTreeMap<usize, usize> = BTreeMap::new();
  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();
  unsafe {
    walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |slot| {
      let slot_addr = slot as usize;
      // SAFETY: The walker only yields aligned pointer slots.
      let value = *(slot as *mut *mut u8) as usize;
      visited.insert(slot_addr, value);
    })
    .expect("walk");
  }

  assert_eq!(visited, expected);
}

#[cfg(target_arch = "x86_64")]
#[test]
fn derived_pointers_are_relocated_from_base() {
  use std::collections::BTreeSet;

  let bytes = build_stackmaps_with_derived_pointer();
  let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps");

  let (callsite_ra, callsite) = stackmaps.iter().next().expect("callsite");
  let stack_size = callsite.stack_size;

  let mut stack = vec![0u8; 512];
  let base = stack.as_mut_ptr() as usize;

  // x86_64 FP_RECORD_SIZE=8.
  let fp_delta = (stack_size - 8) as usize;
  let caller_sp = align_up(base + 256, 16);
  let caller_fp = caller_sp + fp_delta;
  let start_fp = align_up(base + 128, 16);

  unsafe {
    write_u64(start_fp + 0, caller_fp as u64);
    write_u64(start_fp + 8, callsite_ra);

    write_u64(caller_fp + 0, 0);
    write_u64(caller_fp + 8, 0);
  }

  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();

  // Populate the base and derived *value* slots described by the stackmap.
  //
  // Stackmap uses:
  //   base   = [SP + 0]
  //   derived = [SP + 8]
  let base_val = Box::into_raw(Box::new(0u8)) as u64;
  let delta = 8u64;
  unsafe {
    write_u64(caller_sp + 0, base_val);
    write_u64(caller_sp + 8, base_val + delta);
  }

  let mut visited = BTreeSet::<usize>::new();
  unsafe {
    walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |slot| {
      visited.insert(slot as usize);

      // Simulate a moving GC by "relocating" the base pointer in-place. The stack walker should
      // then update the derived slot to preserve the original offset.
      let slot_ptr = slot as *mut *mut u8;
      let old = slot_ptr.read() as u64;
      let new = old + 0x1000;
      slot_ptr.write(new as *mut u8);
    })
    .expect("walk");
  }

  assert_eq!(visited.len(), 1, "expected to visit only the base root slot");
  assert!(
    visited.contains(&(caller_sp + 0)),
    "expected to visit base slot [SP+0]={:#x}, visited={visited:?}",
    caller_sp
  );

  // Derived slot should have been updated based on the relocated base value.
  let base_after = unsafe { read_u64(caller_sp + 0) };
  let derived_after = unsafe { read_u64(caller_sp + 8) };
  assert_eq!(base_after, base_val + 0x1000);
  assert_eq!(derived_after, (base_val + 0x1000) + delta);
}

#[cfg(target_arch = "x86_64")]
#[test]
fn statepoints_with_custom_patchpoint_id_are_walked() {
  use std::collections::BTreeSet;

  let mut bytes = build_stackmaps_with_derived_pointer();
  // StackMaps records store the statepoint ID as `patchpoint_id`. LLVM allows overriding this via
  // the `"statepoint-id"` callsite attribute, so the runtime must not rely on any fixed constant.
  //
  // Offset: header (16) + function record (24) = 40.
  bytes[40..48].copy_from_slice(&42u64.to_le_bytes());

  let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps");
  let (callsite_ra, callsite) = stackmaps.iter().next().expect("callsite");
  let stack_size = callsite.stack_size;

  let mut stack = vec![0u8; 512];
  let base = stack.as_mut_ptr() as usize;

  // x86_64 FP_RECORD_SIZE=8.
  let fp_delta = (stack_size - 8) as usize;
  let caller_sp = align_up(base + 256, 16);
  let caller_fp = caller_sp + fp_delta;
  let start_fp = align_up(base + 128, 16);

  unsafe {
    write_u64(start_fp + 0, caller_fp as u64);
    write_u64(start_fp + 8, callsite_ra);

    write_u64(caller_fp + 0, 0);
    write_u64(caller_fp + 8, 0);
  }

  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();

  // base = [SP + 0], derived = [SP + 8]
  let base_val = Box::into_raw(Box::new(0u8)) as u64;
  let delta = 8u64;
  unsafe {
    write_u64(caller_sp + 0, base_val);
    write_u64(caller_sp + 8, base_val + delta);
  }

  let mut visited = BTreeSet::<usize>::new();
  unsafe {
    walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |slot| {
      visited.insert(slot as usize);

      let slot_ptr = slot as *mut *mut u8;
      let old = slot_ptr.read() as u64;
      slot_ptr.write((old + 0x1000) as *mut u8);
    })
    .expect("walk");
  }

  assert_eq!(visited.len(), 1, "expected to visit only the base root slot");
  assert!(visited.contains(&(caller_sp + 0)));

  let base_after = unsafe { read_u64(caller_sp + 0) };
  let derived_after = unsafe { read_u64(caller_sp + 8) };
  assert_eq!(base_after, base_val + 0x1000);
  assert_eq!(derived_after, base_after + delta);
}

#[cfg(target_arch = "x86_64")]
#[test]
fn null_derived_pointers_remain_null_after_base_relocation() {
  use std::collections::BTreeSet;

  let bytes = build_stackmaps_with_derived_pointer();
  let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps");

  let (callsite_ra, callsite) = stackmaps.iter().next().expect("callsite");
  let stack_size = callsite.stack_size;

  let mut stack = vec![0u8; 512];
  let base = stack.as_mut_ptr() as usize;

  // x86_64 FP_RECORD_SIZE=8.
  let fp_delta = (stack_size - 8) as usize;
  let caller_sp = align_up(base + 256, 16);
  let caller_fp = caller_sp + fp_delta;
  let start_fp = align_up(base + 128, 16);

  unsafe {
    write_u64(start_fp + 0, caller_fp as u64);
    write_u64(start_fp + 8, callsite_ra);
    write_u64(caller_fp + 0, 0);
    write_u64(caller_fp + 8, 0);
  }

  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();

  // Stackmap uses:
  //   base    = [SP + 0]
  //   derived = [SP + 8]
  //
  // Derived is intentionally null; it must remain null after base relocation.
  let base_val = Box::into_raw(Box::new(0u8)) as u64;
  unsafe {
    write_u64(caller_sp + 0, base_val);
    write_u64(caller_sp + 8, 0);
  }

  let mut visited = BTreeSet::<usize>::new();
  unsafe {
    walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |slot| {
      visited.insert(slot as usize);

      // Relocate the base slot in-place.
      let slot_ptr = slot as *mut *mut u8;
      let old = slot_ptr.read() as u64;
      let new = old + 0x1000;
      slot_ptr.write(new as *mut u8);
    })
    .expect("walk");
  }

  assert_eq!(visited.len(), 1, "expected to visit only the base root slot");
  assert!(
    visited.contains(&(caller_sp + 0)),
    "expected to visit base slot [SP+0]={:#x}, visited={visited:?}",
    caller_sp
  );

  let base_after = unsafe { read_u64(caller_sp + 0) };
  let derived_after = unsafe { read_u64(caller_sp + 8) };
  assert_eq!(base_after, base_val + 0x1000);
  assert_eq!(derived_after, 0, "null derived pointer must remain null");
}

#[cfg(target_arch = "x86_64")]
#[test]
fn non_statepoint_records_are_skipped() {
  let mut bytes = build_stackmaps_with_derived_pointer();
  // The stackmap parser runs a debug-mode verifier that (by convention) only checks records with
  // `patchpoint_id == 0xABCDEF00`. Use a different ID so we can construct an obviously non-statepoint
  // record without tripping the verifier.
  //
  // Offset: header (16) + function record (24) = 40.
  bytes[40..48].copy_from_slice(&0x1234_5678u64.to_le_bytes());

  // Overwrite the first location kind so the record no longer matches the LLVM
  // statepoint layout (3 leading constant header locations).
  //
  // Offset:
  //   header (16) + function record (24) + record header (16) = 56
  // First location kind is a single byte at that offset.
  bytes[56] = 1; // Register

  let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps");
  let (callsite_ra, _callsite) = stackmaps.iter().next().expect("callsite");

  let mut stack = vec![0u8; 512];
  let base = stack.as_mut_ptr() as usize;

  let start_fp = align_up(base + 128, 16);
  let caller_fp = align_up(base + 256, 16);

  unsafe {
    write_u64(start_fp + 0, caller_fp as u64);
    write_u64(start_fp + 8, callsite_ra);
    write_u64(caller_fp + 0, 0);
    write_u64(caller_fp + 8, 0);
  }

  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();
  let mut visited = Vec::new();
  unsafe {
    walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |slot| {
      visited.push(slot as usize);
    })
    .expect("walk");
  }
  assert!(visited.is_empty());
}

#[cfg(target_arch = "x86_64")]
#[test]
fn multiple_derived_pointers_share_base_and_are_relocated() {
  use std::collections::BTreeSet;

  let bytes = build_stackmaps_with_two_derived_pointers();
  let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps");

  let (callsite_ra, callsite) = stackmaps.iter().next().expect("callsite");
  let stack_size = callsite.stack_size;

  let mut stack = vec![0u8; 512];
  let base = stack.as_mut_ptr() as usize;

  // x86_64 FP_RECORD_SIZE=8.
  let fp_delta = (stack_size - 8) as usize;
  let caller_sp = align_up(base + 256, 16);
  let caller_fp = caller_sp + fp_delta;
  let start_fp = align_up(base + 128, 16);

  unsafe {
    write_u64(start_fp + 0, caller_fp as u64);
    write_u64(start_fp + 8, callsite_ra);

    write_u64(caller_fp + 0, 0);
    write_u64(caller_fp + 8, 0);
  }

  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();

  // Stackmap uses:
  //   base     = [SP + 0]
  //   derived1 = [SP + 8]
  //   derived2 = [SP + 16]
  let base_val = Box::into_raw(Box::new(0u8)) as u64;
  unsafe {
    write_u64(caller_sp + 0, base_val);
    write_u64(caller_sp + 8, base_val + 8);
    write_u64(caller_sp + 16, base_val + 16);
  }

  let mut visited = BTreeSet::<usize>::new();
  unsafe {
    walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |slot| {
      visited.insert(slot as usize);

      // Simulate a moving GC by relocating the base pointer in-place.
      let slot_ptr = slot as *mut *mut u8;
      let old = slot_ptr.read() as u64;
      slot_ptr.write((old + 0x1000) as *mut u8);
    })
    .expect("walk");
  }

  assert_eq!(visited.len(), 1, "expected to visit only the base root slot");
  assert!(
    visited.contains(&(caller_sp + 0)),
    "expected to visit base slot [SP+0]={:#x}, visited={visited:?}",
    caller_sp
  );

  let base_after = unsafe { read_u64(caller_sp + 0) };
  assert_eq!(base_after, base_val + 0x1000);
  assert_eq!(unsafe { read_u64(caller_sp + 8) }, base_after + 8);
  assert_eq!(unsafe { read_u64(caller_sp + 16) }, base_after + 16);
}

fn align_up(v: usize, align: usize) -> usize {
  (v + (align - 1)) & !(align - 1)
}

unsafe fn write_u64(addr: usize, val: u64) {
  (addr as *mut u64).write_unaligned(val);
}

unsafe fn read_u64(addr: usize) -> u64 {
  (addr as *const u64).read_unaligned()
}

fn add_signed_u64(base: u64, offset: i32) -> Option<u64> {
  if offset >= 0 {
    base.checked_add(offset as u64)
  } else {
    base.checked_sub((-offset) as u64)
  }
}

#[cfg(target_arch = "x86_64")]
fn build_stackmaps_with_derived_pointer() -> Vec<u8> {
  build_stackmaps_with_shared_base_derived_offsets(&[8])
}

#[cfg(target_arch = "x86_64")]
fn build_stackmaps_with_two_derived_pointers() -> Vec<u8> {
  build_stackmaps_with_shared_base_derived_offsets(&[8, 16])
}

#[cfg(target_arch = "x86_64")]
fn build_stackmaps_with_shared_base_derived_offsets(derived_offsets: &[i32]) -> Vec<u8> {
  // Minimal stackmap section containing one callsite record with one or more derived-pointer pairs
  // that all share the same base slot ([SP + 0]).
  //
  // This is used to assert the stack walker can:
  // - relocate the base slot once, and
  // - update each derived slot to preserve its original offset from the base.
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
  out.extend_from_slice(&40u64.to_le_bytes()); // stack_size
  out.extend_from_slice(&1u64.to_le_bytes()); // record_count

  // One record.
  out.extend_from_slice(&0xabcdef00u64.to_le_bytes()); // patchpoint_id
  out.extend_from_slice(&0x10u32.to_le_bytes()); // instruction_offset
  out.extend_from_slice(&0u16.to_le_bytes()); // reserved
  let num_locations = 3usize + derived_offsets.len() * 2;
  out.extend_from_slice(&u16::try_from(num_locations).unwrap().to_le_bytes());

  // 3 leading constants (statepoint header).
  for _ in 0..3 {
    out.extend_from_slice(&[4, 0]); // Constant, reserved
    out.extend_from_slice(&8u16.to_le_bytes()); // size
    out.extend_from_slice(&0u16.to_le_bytes()); // dwarf_reg
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&0i32.to_le_bytes()); // small const
  }

  for &derived_off in derived_offsets {
    // base: Indirect [SP + 0]
    out.extend_from_slice(&[3, 0]); // Indirect, reserved
    out.extend_from_slice(&8u16.to_le_bytes()); // size
    out.extend_from_slice(&7u16.to_le_bytes()); // dwarf_reg (x86_64 SP)
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&0i32.to_le_bytes()); // offset

    // derived: Indirect [SP + derived_off] (different slot => derived pointer)
    out.extend_from_slice(&[3, 0]); // Indirect, reserved
    out.extend_from_slice(&8u16.to_le_bytes()); // size
    out.extend_from_slice(&7u16.to_le_bytes()); // dwarf_reg (x86_64 SP)
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&derived_off.to_le_bytes()); // offset
  }

  // Align to 8.
  while out.len() % 8 != 0 {
    out.push(0);
  }

  // LiveOuts (none).
  out.extend_from_slice(&0u16.to_le_bytes()); // num_live_outs
  out.extend_from_slice(&0u16.to_le_bytes()); // reserved

  // Align to 8.
  while out.len() % 8 != 0 {
    out.push(0);
  }

  out
}

#[cfg(target_arch = "aarch64")]
#[test]
fn synthetic_stack_enumerates_roots_from_stackmaps() {
  use runtime_native::stackmaps::Location;
  use runtime_native::statepoints::StatepointRecord;
  use std::collections::BTreeMap;

  let stackmaps = StackMaps::parse(include_bytes!("fixtures/bin/statepoint_aarch64.bin"))
    .expect("parse stackmaps");

  let (callsite_ra, callsite) = stackmaps.iter().next().expect("non-empty");
  let stack_size = callsite.stack_size;

  // Fake stack memory.
  let mut stack = vec![0u8; 1024];
  let base = stack.as_mut_ptr() as usize;

  // AArch64 FP_RECORD_SIZE=16 (saved X29+X30).
  let fp_delta = (stack_size - 16) as usize;
  let caller_sp = align_up(base + 512, 16);
  let caller_fp = caller_sp + fp_delta;
  let start_fp = align_up(base + 256, 16);

  unsafe {
    // runtime frame -> caller
    write_u64(start_fp + 0, caller_fp as u64);
    write_u64(start_fp + 8, callsite_ra);

    // caller -> null
    write_u64(caller_fp + 0, 0);
    write_u64(caller_fp + 8, 0);
  }

  let statepoint = StatepointRecord::new(callsite.record).expect("decode statepoint layout");
  let mut expected: BTreeMap<usize, usize> = BTreeMap::new();

  let mut slots: Vec<usize> = Vec::new();
  for pair in statepoint.gc_pairs() {
    for loc in [&pair.base, &pair.derived] {
      match loc {
        Location::Indirect { dwarf_reg, offset, .. } => {
          assert_eq!(*dwarf_reg, 31, "fixture roots must be [SP + off]");
          let slot_addr = add_signed_u64(caller_sp as u64, *offset).expect("slot addr");
          slots.push(slot_addr as usize);
        }
        other => panic!("unexpected root location kind in fixture: {other:?}"),
      }
    }
  }
  slots.sort_unstable();
  slots.dedup();
  for slot_addr in slots {
    let obj = Box::into_raw(Box::new(0u8)) as usize;
    unsafe {
      write_u64(slot_addr, obj as u64);
    }
    expected.insert(slot_addr, obj);
  }

  let mut visited: BTreeMap<usize, usize> = BTreeMap::new();
  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();
  unsafe {
    walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |slot| {
      let slot_addr = slot as usize;
      let value = *(slot as *mut *mut u8) as usize;
      visited.insert(slot_addr, value);
    })
    .expect("walk");
  }

  assert_eq!(visited, expected);
}
