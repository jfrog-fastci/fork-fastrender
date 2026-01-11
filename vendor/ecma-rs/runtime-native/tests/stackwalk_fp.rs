#[cfg(target_arch = "x86_64")]
use runtime_native::{walk_gc_root_pairs_from_fp, walk_gc_roots_from_fp, StackMaps, WalkError};

#[cfg(target_arch = "x86_64")]
use runtime_native::stackmaps::StackSize;

#[cfg(target_arch = "aarch64")]
use runtime_native::{walk_gc_roots_from_fp, StackMaps};

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
use runtime_native::stackwalk::StackBounds;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
use runtime_native::test_util::TestRuntimeGuard;

#[cfg(target_arch = "x86_64")]
#[test]
fn synthetic_stack_enumerates_roots_from_stackmaps() {
  let _rt = TestRuntimeGuard::new();
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

  // Fake stack memory.
  let mut stack = vec![0u8; 4096];
  let base = stack.as_mut_ptr() as usize;

  // This test models the statepoint ABI the runtime relies on:
  //
  // - Stackmap `Indirect [SP + off]` locations are based on the *callsite* SP in the caller.
  // - `stack_size` in the stackmap function record is *not* the callsite SP when the caller pushes
  //   outgoing stack arguments (or otherwise adjusts SP around a call).
  // - With frame pointers enabled, we recover the caller callsite SP from the callee frame pointer:
  //     caller_sp_callsite = callee_fp + 16
  //
  // We construct a 2-frame managed call chain:
  //   runtime_frame (start_fp) -> caller1_fp -> caller2_fp -> null
  let start_fp = align_up(base + 0x100, 16);
  let caller1_fp = align_up(base + 0x600, 16);
  let caller2_fp = align_up(base + 0xB00, 16);
 
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
 
  // Regression guard: ensure our synthetic frame pointers model a callsite with extra SP
  // adjustment (e.g. outgoing stack args) such that the old `fp - stack_size` reconstruction would
  // be wrong for non-top frames.
  let StackSize::Known(stack_size) = callsites[1].1.stack_size else {
    panic!("fixture callsites should have a known stack_size");
  };
  let old_locals = stack_size.checked_sub(8).expect("stack_size < FP_RECORD_SIZE");
  let old_sp = (caller2_fp as u64)
    .checked_sub(old_locals)
    .expect("old SP estimate underflow");
  assert_ne!(
    old_sp, caller2_sp_callsite as u64,
    "test requires callsite SP to differ from stack_size-based estimate"
  );

  // Fill each unique root slot in each frame with a distinct pointer value, and
  // record the expected slot->value mapping.
  let mut expected: BTreeMap<usize, usize> = BTreeMap::new();
  for (frame_sp, callsite) in [
    (caller1_sp_callsite, callsites[0].1),
    (caller2_sp_callsite, callsites[1].1),
  ] {
    let statepoint = StatepointRecord::new(callsite.record).expect("decode statepoint layout");

    let mut slots: Vec<usize> = Vec::new();
    for pair in statepoint.gc_pairs() {
      let loc = &pair.base;
      match loc {
        Location::Indirect { dwarf_reg, offset, .. } => {
          assert_eq!(*dwarf_reg, 7, "fixture roots must be [SP + off]");
          let slot_addr = add_signed_u64(frame_sp as u64, *offset).expect("slot addr");
          slots.push(slot_addr as usize);
        }
        other => panic!("unexpected root location kind in fixture: {other:?}"),
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

  let _rt = TestRuntimeGuard::new();
  let bytes = build_stackmaps_with_derived_pointer();
  let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps");

  let (callsite_ra, callsite) = stackmaps.iter().next().expect("callsite");
  let _stack_size = callsite.stack_size;

  let mut stack = vec![0u8; 512];
  let base = stack.as_mut_ptr() as usize;

  let caller_fp = align_up(base + 256, 16);
  let start_fp = align_up(base + 128, 16);
  let caller_sp_callsite = start_fp + 16;

  unsafe {
    // runtime frame -> caller
    write_u64(start_fp + 0, caller_fp as u64);
    write_u64(start_fp + 8, callsite_ra);

    // caller -> null
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
    write_u64(caller_sp_callsite + 0, base_val);
    write_u64(caller_sp_callsite + 8, base_val + delta);
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
    visited.contains(&(caller_sp_callsite + 0)),
    "expected to visit base slot [SP+0]={:#x}, visited={visited:?}",
    caller_sp_callsite
  );

  // Derived slot should have been updated based on the relocated base value.
  let base_after = unsafe { read_u64(caller_sp_callsite + 0) };
  let derived_after = unsafe { read_u64(caller_sp_callsite + 8) };
  assert_eq!(base_after, base_val + 0x1000);
  assert_eq!(derived_after, (base_val + 0x1000) + delta);
}

#[cfg(target_arch = "x86_64")]
#[test]
fn derived_pointers_are_traced_via_base_and_relocated_via_pairs() {
  use runtime_native::arch::SafepointContext;
  use runtime_native::gc_roots::relocate_reloc_pairs_in_place;
  use runtime_native::stackwalk_fp::{
    walk_gc_reloc_pairs_from_safepoint_context, walk_gc_roots_from_safepoint_context,
  };
  use runtime_native::statepoints::RootSlot;
  use stackmap_context::ThreadContext;

  let bytes = build_stackmaps_with_derived_pointer();
  let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps");

  let (callsite_ra, _callsite) = stackmaps.iter().next().expect("callsite");

  let mut stack = vec![0u8; 512];
  let base = stack.as_mut_ptr() as usize;

  let caller_fp = align_up(base + 256, 16);
  let start_fp = align_up(base + 128, 16);
  let caller_sp_callsite = start_fp + 16;

  unsafe {
    // caller -> null
    write_u64(caller_fp + 0, 0);
    write_u64(caller_fp + 8, 0);
  }

  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();

  // Stackmap uses:
  //   base   = [SP + 0]
  //   derived = [SP + 8]
  let old_base = 0x1000usize;
  let offset = 0x10usize;
  unsafe {
    write_u64(caller_sp_callsite + 0, old_base as u64);
    write_u64(caller_sp_callsite + 8, (old_base + offset) as u64);
  }

  let ctx = SafepointContext {
    sp_entry: caller_sp_callsite - 8,
    sp: caller_sp_callsite,
    fp: caller_fp,
    ip: callsite_ra as usize,
  };

  // Marking/tracing uses base slots only.
  let mut root_slots: Vec<usize> = Vec::new();
  unsafe {
    walk_gc_roots_from_safepoint_context(&ctx, Some(bounds), &stackmaps, |slot| {
      root_slots.push(slot as usize);
    })
    .expect("walk roots");
  }
  root_slots.sort_unstable();
  assert_eq!(root_slots, vec![caller_sp_callsite]);

  // Relocation uses (base, derived) slot pairs.
  let mut pairs: Vec<runtime_native::gc_roots::RelocPair> = Vec::new();
  unsafe {
    walk_gc_reloc_pairs_from_safepoint_context(&ctx, Some(bounds), &stackmaps, |pair| {
      pairs.push(pair);
    })
    .expect("walk reloc pairs");
  }
  assert_eq!(pairs.len(), 1);
  let pair0 = pairs[0];
  match pair0.base_slot {
    RootSlot::StackAddr(p) => assert_eq!(p as usize, caller_sp_callsite),
    other => panic!("expected stack slot for base, got {other:?}"),
  }
  match pair0.derived_slot {
    RootSlot::StackAddr(p) => assert_eq!(p as usize, caller_sp_callsite + 8),
    other => panic!("expected stack slot for derived, got {other:?}"),
  }

  let mut ctx = ThreadContext::default();
  relocate_reloc_pairs_in_place(&mut ctx, pairs, |old| old + 0x1000);
  assert_eq!(pair0.base_slot.read_u64(&ctx), 0x2000);
  assert_eq!(pair0.derived_slot.read_u64(&ctx), 0x2000 + offset as u64);
}

#[cfg(target_arch = "x86_64")]
#[test]
fn synthetic_stack_enumerates_root_pairs_from_stackmaps() {
  use std::collections::BTreeSet;

  let bytes = build_stackmaps_with_two_records_one_derived_pointer();
  let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps");

  // Pick two callsite records so we can build a multi-frame managed call chain.
  let callsites: Vec<(u64, runtime_native::stackmaps::CallSite<'_>)> =
    stackmaps.iter().take(2).collect();
  assert_eq!(callsites.len(), 2);

  let mut stack = vec![0u8; 1024];
  let base = stack.as_mut_ptr() as usize;

  let start_fp = align_up(base + 0x100, 16);
  let caller1_fp = align_up(base + 0x200, 16);
  let caller2_fp = align_up(base + 0x300, 16);

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

    // Fill the slots so the walker can safely read them (even though this API
    // only reports addresses).
    write_u64(caller1_sp_callsite + 0, 0x1111);
    write_u64(caller1_sp_callsite + 8, 0x2222);
    write_u64(caller2_sp_callsite + 0, 0x3333);
    write_u64(caller2_sp_callsite + 8, 0x4444);
  }

  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();
  let mut visited = BTreeSet::<(usize, usize)>::new();
  unsafe {
    walk_gc_root_pairs_from_fp(start_fp as u64, Some(bounds), &stackmaps, |_return_addr, pairs| {
      for &(base_slot, derived_slot) in pairs {
        visited.insert((base_slot as usize, derived_slot as usize));
      }
    })
    .expect("walk");
  }

  let expected = BTreeSet::from([
    (caller1_sp_callsite + 0, caller1_sp_callsite + 8),
    (caller2_sp_callsite + 0, caller2_sp_callsite + 8),
  ]);
  assert_eq!(visited, expected);
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
  let _stack_size = callsite.stack_size;

  let mut stack = vec![0u8; 512];
  let base = stack.as_mut_ptr() as usize;

  let caller_fp = align_up(base + 256, 16);
  let start_fp = align_up(base + 128, 16);
  let caller_sp_callsite = start_fp + 16;

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
    write_u64(caller_sp_callsite + 0, base_val);
    write_u64(caller_sp_callsite + 8, base_val + delta);
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
  assert!(
    visited.contains(&(caller_sp_callsite + 0)),
    "expected to visit base slot [SP+0]={:#x}, visited={visited:?}",
    caller_sp_callsite
  );

  let base_after = unsafe { read_u64(caller_sp_callsite + 0) };
  let derived_after = unsafe { read_u64(caller_sp_callsite + 8) };
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
  let _stack_size = callsite.stack_size;

  let mut stack = vec![0u8; 512];
  let base = stack.as_mut_ptr() as usize;

  let caller_fp = align_up(base + 256, 16);
  let start_fp = align_up(base + 128, 16);
  let caller_sp_callsite = start_fp + 16;

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
    write_u64(caller_sp_callsite + 0, base_val);
    write_u64(caller_sp_callsite + 8, 0);
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
    visited.contains(&(caller_sp_callsite + 0)),
    "expected to visit base slot [SP+0]={:#x}, visited={visited:?}",
    caller_sp_callsite
  );

  let base_after = unsafe { read_u64(caller_sp_callsite + 0) };
  let derived_after = unsafe { read_u64(caller_sp_callsite + 8) };
  assert_eq!(base_after, base_val + 0x1000);
  assert_eq!(derived_after, 0, "null derived pointer must remain null");
}

#[cfg(target_arch = "x86_64")]
#[test]
fn derived_pointers_are_nulled_when_base_is_cleared() {
  use std::collections::BTreeSet;

  let bytes = build_stackmaps_with_derived_pointer();
  let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps");

  let (callsite_ra, callsite) = stackmaps.iter().next().expect("callsite");
  let _stack_size = callsite.stack_size;

  let mut stack = vec![0u8; 512];
  let base = stack.as_mut_ptr() as usize;

  let caller_fp = align_up(base + 256, 16);
  let start_fp = align_up(base + 128, 16);
  let caller_sp_callsite = start_fp + 16;

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
  let base_val = Box::into_raw(Box::new(0u8)) as u64;
  unsafe {
    write_u64(caller_sp_callsite + 0, base_val);
    write_u64(caller_sp_callsite + 8, base_val + 8);
  }

  let mut visited = BTreeSet::<usize>::new();
  unsafe {
    walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |slot| {
      visited.insert(slot as usize);

      // Simulate a collector clearing the root slot (e.g. dead after marking).
      let slot_ptr = slot as *mut *mut u8;
      slot_ptr.write(std::ptr::null_mut());
    })
    .expect("walk");
  }

  assert_eq!(visited.len(), 1, "expected to visit only the base root slot");
  assert!(
    visited.contains(&(caller_sp_callsite + 0)),
    "expected to visit base slot [SP+0]={:#x}, visited={visited:?}",
    caller_sp_callsite
  );

  let base_after = unsafe { read_u64(caller_sp_callsite + 0) };
  let derived_after = unsafe { read_u64(caller_sp_callsite + 8) };
  assert_eq!(base_after, 0);
  assert_eq!(
    derived_after, 0,
    "derived pointer must be nulled when the base is cleared"
  );
}

#[cfg(target_arch = "x86_64")]
#[test]
#[cfg(any(debug_assertions, feature = "conservative_roots"))]
fn missing_stackmap_uses_conservative_fallback_scan() {
  use runtime_native::gc::ObjHeader;
  use runtime_native::test_util::TestGcGuard;
  use runtime_native::{GcHeap, TypeDescriptor};
  use std::sync::atomic::AtomicUsize;

  // The conservative fallback uses the type-descriptor registry (debug builds
  // and/or `conservative_roots`) to filter candidate pointers down to likely
  // object headers.
  static DESC: TypeDescriptor = TypeDescriptor::new(core::mem::size_of::<ObjHeader>(), &[]);
  let mut heap = GcHeap::new();
  let _ = heap.alloc_old(&DESC);

  // Set up a fake young-space range containing a synthetic object header.
  let heap_bytes: Box<[u8; 256]> = Box::new([0; 256]);
  let _gc_guard = TestGcGuard::new();

  let heap_start = heap_bytes.as_ptr().cast_mut();
  let heap_end = unsafe { heap_start.add(heap_bytes.len()) };
  runtime_native::rt_gc_set_young_range(heap_start, heap_end);

  let hdr_align = core::mem::align_of::<ObjHeader>();
  let hdr_size = core::mem::size_of::<ObjHeader>();
  let obj_addr = align_up(heap_start as usize, hdr_align);
  assert!(
    obj_addr + hdr_size <= heap_end as usize,
    "fake heap not large enough for ObjHeader"
  );
  let obj_ptr = obj_addr as *mut u8;

  unsafe {
    // ObjHeader::type_desc is at offset 0 (repr(C)).
    (obj_ptr as *mut *const TypeDescriptor).write(&DESC as *const TypeDescriptor);
    // ObjHeader::meta follows the type descriptor pointer.
    let meta_ptr = obj_ptr.add(core::mem::size_of::<*const TypeDescriptor>()) as *mut AtomicUsize;
    meta_ptr.write(AtomicUsize::new(0));
  }

  let stackmaps = StackMaps::parse(include_bytes!("fixtures/bin/statepoint_x86_64.bin"))
    .expect("parse stackmaps");

  // Ensure the chosen return address is not present in the stackmaps index so
  // the FP walker uses the conservative fallback.
  let missing_ra = 0x5555_u64;
  assert!(stackmaps.lookup(missing_ra).is_none());

  let mut stack = vec![0u8; 512];
  let base = stack.as_mut_ptr() as usize;

  let start_fp = align_up(base + 128, 16);
  let caller_fp = align_up(base + 256, 16);
  let caller_sp = start_fp + 16;
  let slot_addr = caller_sp + 32;

  unsafe {
    // runtime frame -> caller frame (no stackmap record)
    write_u64(start_fp + 0, caller_fp as u64);
    write_u64(start_fp + 8, missing_ra);

    // caller frame -> end
    write_u64(caller_fp + 0, 0);
    write_u64(caller_fp + 8, 0);

    // Place the candidate pointer into the scanned range [caller_sp, caller_fp).
    write_u64(slot_addr, obj_ptr as u64);
  }

  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();
  let mut visited: Vec<usize> = Vec::new();
  unsafe {
    walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |slot| {
      visited.push(slot as usize);
    })
    .expect("walk");
  }

  assert_eq!(visited, vec![slot_addr]);
}

#[cfg(target_arch = "x86_64")]
#[test]
fn non_statepoint_records_are_skipped() {
  let mut bytes = build_stackmaps_with_derived_pointer();

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
fn statepoint_prefix_with_invalid_layout_is_skipped() {
  // Construct a record that starts with the statepoint-style 3-constant header, but uses an invalid
  // `deopt_count` so decoding as a `gc.statepoint` fails. The FP walker should treat it as a
  // non-statepoint record and skip it (rather than erroring while attempting to decode).
  let bytes = build_stackmaps_with_invalid_statepoint_deopt_count();
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
  let _stack_size = callsite.stack_size;

  let mut stack = vec![0u8; 512];
  let base = stack.as_mut_ptr() as usize;

  let caller_fp = align_up(base + 256, 16);
  let start_fp = align_up(base + 128, 16);
  let caller_sp_callsite = start_fp + 16;

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
    write_u64(caller_sp_callsite + 0, base_val);
    write_u64(caller_sp_callsite + 8, base_val + 8);
    write_u64(caller_sp_callsite + 16, base_val + 16);
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
    visited.contains(&(caller_sp_callsite + 0)),
    "expected to visit base slot [SP+0]={:#x}, visited={visited:?}",
    caller_sp_callsite
  );

  let base_after = unsafe { read_u64(caller_sp_callsite + 0) };
  assert_eq!(base_after, base_val + 0x1000);
  assert_eq!(unsafe { read_u64(caller_sp_callsite + 8) }, base_after + 8);
  assert_eq!(unsafe { read_u64(caller_sp_callsite + 16) }, base_after + 16);
}

#[cfg(target_arch = "x86_64")]
#[test]
fn out_of_bounds_start_fp_is_rejected() {
  let bytes = build_stackmaps_with_derived_pointer();
  let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps");

  let mut stack = vec![0u8; 128];
  let base = stack.as_mut_ptr() as usize;
  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();

  // Point completely outside the synthetic stack range.
  let start_fp = align_up(base + stack.len() + 64, 16);

  let res = unsafe { walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |_| {}) };
  assert!(matches!(res, Err(WalkError::FramePointerOutOfBounds { .. })));
}

#[cfg(target_arch = "x86_64")]
#[test]
fn non_monotonic_fp_chain_is_rejected() {
  let bytes = build_stackmaps_with_derived_pointer();
  let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps");

  let mut stack = vec![0u8; 128];
  let base = stack.as_mut_ptr() as usize;
  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();

  let start_fp = align_up(base + 32, 16);
  unsafe {
    // Loop back to itself.
    write_u64(start_fp + 0, start_fp as u64);
    write_u64(start_fp + 8, 0x1234);
  }

  let res = unsafe { walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |_| {}) };
  assert!(matches!(res, Err(WalkError::NonMonotonicFp { .. })));
}

#[cfg(target_arch = "x86_64")]
#[test]
fn out_of_bounds_caller_fp_is_rejected() {
  let bytes = build_stackmaps_with_derived_pointer();
  let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps");

  let mut stack = vec![0u8; 128];
  let base = stack.as_mut_ptr() as usize;
  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();

  let start_fp = align_up(base + 32, 16);
  // Older frames live at higher addresses; pick an aligned pointer above `start_fp` but outside bounds.
  let caller_fp = align_up(base + stack.len() + 64, 16);
  unsafe {
    write_u64(start_fp + 0, caller_fp as u64);
    write_u64(start_fp + 8, 0x1234);
  }

  let res = unsafe { walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |_| {}) };
  assert!(matches!(res, Err(WalkError::FramePointerOutOfBounds { .. })));
}

#[cfg(target_arch = "x86_64")]
#[test]
fn out_of_bounds_caller_sp_is_rejected() {
  use runtime_native::arch::SafepointContext;
  use runtime_native::stackwalk_fp::walk_gc_roots_from_safepoint_context;

  // Ensure we reject a top-frame `caller_sp` that is outside the provided stack
  // bounds (this can happen if bounds capture is wrong or a context is corrupt).
  let bytes = build_stackmaps_with_derived_pointer();
  let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps");
  let (callsite_ra, _callsite) = stackmaps.iter().next().expect("callsite");

  let mut stack = vec![0u8; 512];
  let base = stack.as_mut_ptr() as usize;
  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();

  let caller_fp = align_up(base + 256, 16);
  unsafe {
    write_u64(caller_fp + 0, 0);
    write_u64(caller_fp + 8, 0);
  }

  // Point the captured SP above the synthetic stack range.
  let caller_sp = (base + stack.len() + 64) as usize;
  let ctx = SafepointContext {
    sp_entry: caller_sp,
    sp: caller_sp,
    fp: caller_fp,
    ip: callsite_ra as usize,
  };
  let res = unsafe { walk_gc_roots_from_safepoint_context(&ctx, Some(bounds), &stackmaps, |_| {}) };
  assert!(matches!(res, Err(WalkError::StackPointerOutOfBounds { .. })));
}

#[cfg(target_arch = "x86_64")]
#[test]
fn misaligned_root_slot_is_rejected() {
  let bytes = build_stackmaps_with_shared_base_derived_offsets(&[1]);
  let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps");
  let (callsite_ra, _callsite) = stackmaps.iter().next().expect("callsite");

  let mut stack = vec![0u8; 512];
  let base = stack.as_mut_ptr() as usize;

  let start_fp = align_up(base + 128, 16);
  let caller_fp = align_up(base + 256, 16);
  let caller_sp_callsite = start_fp + 16;

  unsafe {
    write_u64(start_fp + 0, caller_fp as u64);
    write_u64(start_fp + 8, callsite_ra);
    write_u64(caller_fp + 0, 0);
    write_u64(caller_fp + 8, 0);
    // Base slot is [SP + 0].
    write_u64(caller_sp_callsite + 0, 0xdead_beef);
  }

  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();
  let res = unsafe { walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |_| {}) };
  assert!(matches!(res, Err(WalkError::MisalignedRootSlot { .. })));
}

fn align_up(v: usize, align: usize) -> usize {
  (v + (align - 1)) & !(align - 1)
}

unsafe fn write_u64(addr: usize, val: u64) {
  (addr as *mut u64).write_unaligned(val);
}

#[cfg(target_arch = "x86_64")]
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
fn build_stackmaps_with_two_records_one_derived_pointer() -> Vec<u8> {
  // Like `build_stackmaps_with_shared_base_derived_offsets`, but emits *two* callsite records so we
  // can construct a multi-frame synthetic call chain.
  let mut out = Vec::new();

  // Header.
  out.push(3); // version
  out.push(0); // reserved0
  out.extend_from_slice(&0u16.to_le_bytes()); // reserved1
  out.extend_from_slice(&1u32.to_le_bytes()); // num_functions
  out.extend_from_slice(&0u32.to_le_bytes()); // num_constants
  out.extend_from_slice(&2u32.to_le_bytes()); // num_records

  // One function record.
  out.extend_from_slice(&0x1000u64.to_le_bytes()); // address
  out.extend_from_slice(&40u64.to_le_bytes()); // stack_size
  out.extend_from_slice(&2u64.to_le_bytes()); // record_count

  for instruction_offset in [0x10u32, 0x20u32] {
    // One record.
    out.extend_from_slice(&0xabcdef00u64.to_le_bytes()); // patchpoint_id
    out.extend_from_slice(&instruction_offset.to_le_bytes()); // instruction_offset
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&5u16.to_le_bytes()); // num_locations (3 header consts + 1 pair)

    // 3 leading constants (statepoint header).
    for _ in 0..3 {
      out.extend_from_slice(&[4, 0]); // Constant, reserved
      out.extend_from_slice(&8u16.to_le_bytes()); // size
      out.extend_from_slice(&0u16.to_le_bytes()); // dwarf_reg
      out.extend_from_slice(&0u16.to_le_bytes()); // reserved
      out.extend_from_slice(&0i32.to_le_bytes()); // small const
    }

    // base: Indirect [SP + 0]
    out.extend_from_slice(&[3, 0]); // Indirect, reserved
    out.extend_from_slice(&8u16.to_le_bytes()); // size
    out.extend_from_slice(&7u16.to_le_bytes()); // dwarf_reg (x86_64 SP)
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&0i32.to_le_bytes()); // offset

    // derived: Indirect [SP + 8]
    out.extend_from_slice(&[3, 0]); // Indirect, reserved
    out.extend_from_slice(&8u16.to_le_bytes()); // size
    out.extend_from_slice(&7u16.to_le_bytes()); // dwarf_reg (x86_64 SP)
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&8i32.to_le_bytes()); // offset

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
  }

  out
}

#[cfg(target_arch = "x86_64")]
fn build_stackmaps_with_invalid_statepoint_deopt_count() -> Vec<u8> {
  // StackMap v3 blob with one record:
  // - 3 constant header locations (statepoint prefix)
  // - 2 Indirect locations
  //
  // The third header constant (`deopt_count`) is set so large that the record cannot be decoded as
  // a valid statepoint.
  let mut out = Vec::new();

  // Header.
  out.push(3); // version
  out.push(0); // reserved0
  out.extend_from_slice(&0u16.to_le_bytes()); // reserved1
  out.extend_from_slice(&1u32.to_le_bytes()); // num_functions
  out.extend_from_slice(&0u32.to_le_bytes()); // num_constants
  out.extend_from_slice(&1u32.to_le_bytes()); // num_records

  // One function record.
  out.extend_from_slice(&0u64.to_le_bytes()); // address
  out.extend_from_slice(&32u64.to_le_bytes()); // stack_size
  out.extend_from_slice(&1u64.to_le_bytes()); // record_count

  // One record.
  out.extend_from_slice(&0x1234u64.to_le_bytes()); // patchpoint_id
  out.extend_from_slice(&0x1234u32.to_le_bytes()); // instruction_offset => callsite pc = 0x1234
  out.extend_from_slice(&0u16.to_le_bytes()); // reserved
  out.extend_from_slice(&5u16.to_le_bytes()); // num_locations

  // callconv=0, flags=0, deopt_count=100 (invalid: exceeds locations).
  for &val in &[0i32, 0i32, 100i32] {
    out.extend_from_slice(&[4, 0]); // Constant, reserved
    out.extend_from_slice(&8u16.to_le_bytes()); // size
    out.extend_from_slice(&0u16.to_le_bytes()); // dwarf_reg
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&val.to_le_bytes()); // small const
  }

  // Two Indirect locations (these are *not* gc-live pairs; they only exist so the record looks
  // plausible under the non-statepoint scanning/validation paths).
  for &off in &[0i32, 8i32] {
    out.extend_from_slice(&[3, 0]); // Indirect, reserved
    out.extend_from_slice(&8u16.to_le_bytes()); // size
    out.extend_from_slice(&7u16.to_le_bytes()); // dwarf_reg (x86_64 SP)
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&off.to_le_bytes()); // offset
  }

  // Align to 8.
  while out.len() % 8 != 0 {
    out.push(0);
  }

  // LiveOuts (none).
  out.extend_from_slice(&0u16.to_le_bytes()); // padding
  out.extend_from_slice(&0u16.to_le_bytes()); // num_live_outs

  // Record trailer aligned to 8 bytes.
  while out.len() % 8 != 0 {
    out.push(0);
  }

  out
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

  // Align to 8 before the live-out header.
  while out.len() % 8 != 0 {
    out.push(0);
  }

  // Live-out header: (padding, num_live_outs). For tests we keep both 0.
  out.extend_from_slice(&0u16.to_le_bytes());
  out.extend_from_slice(&0u16.to_le_bytes());

  // Record ends aligned to 8.
  while out.len() % 8 != 0 {
    out.push(0);
  }
 
  out
}

#[cfg(target_arch = "aarch64")]
#[test]
fn synthetic_stack_enumerates_roots_from_stackmaps() {
  let _rt = TestRuntimeGuard::new();
  use runtime_native::stackmaps::Location;
  use runtime_native::statepoints::StatepointRecord;
  use std::collections::BTreeMap;

  let stackmaps = StackMaps::parse(include_bytes!("fixtures/bin/statepoint_aarch64.bin"))
    .expect("parse stackmaps");

  let (callsite_ra, callsite) = stackmaps.iter().next().expect("non-empty");
  let _stack_size = callsite.stack_size;

  // Fake stack memory.
  let mut stack = vec![0u8; 2048];
  let base = stack.as_mut_ptr() as usize;

  let start_fp = align_up(base + 0x100, 16);
  let caller_fp = align_up(base + 0x300, 16);
  let caller_sp_callsite = start_fp + 16;

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
    let loc = &pair.base;
    match loc {
      Location::Indirect { dwarf_reg, offset, .. } => {
        assert_eq!(*dwarf_reg, 31, "fixture roots must be [SP + off]");
        let slot_addr = add_signed_u64(caller_sp_callsite as u64, *offset).expect("slot addr");
        slots.push(slot_addr as usize);
      }
      other => panic!("unexpected root location kind in fixture: {other:?}"),
    };
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
