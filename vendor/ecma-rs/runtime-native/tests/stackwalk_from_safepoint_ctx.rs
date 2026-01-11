#[cfg(target_arch = "x86_64")]
mod x86_64 {
  use runtime_native::arch::SafepointContext;
  use runtime_native::stackmaps::{Location, StackSize};
  use runtime_native::stackwalk::StackBounds;
  use runtime_native::stackwalk_fp::walk_gc_roots_from_safepoint_context;
  use runtime_native::statepoints::{StatepointRecord, X86_64_DWARF_REG_SP};
  use runtime_native::test_util::TestRuntimeGuard;
  use runtime_native::StackMaps;

  #[test]
  fn synthetic_stack_enumerates_roots_from_safepoint_context() {
    let _rt = TestRuntimeGuard::new();
    let stackmaps = StackMaps::parse(include_bytes!("fixtures/bin/statepoint_x86_64.bin"))
      .expect("parse stackmaps");
    // Pick the first callsite record (BTreeMap iteration is sorted).
    let (callsite_ra, callsite) = stackmaps.iter().next().expect("non-empty");
    let statepoint = StatepointRecord::new(callsite.record).expect("decode statepoint layout");

    // Fake stack memory.
    let mut stack = vec![0u8; 4096];
    let base = stack.as_mut_ptr() as usize;

    // Single managed frame (terminal, caller_fp->0).
    let caller_fp = align_up(base + 3072, 16);
    unsafe {
      write_u64(caller_fp + 0, 0);
      write_u64(caller_fp + 8, 0);
    }

    let ctx = SafepointContext {
      fp: caller_fp,
      ip: callsite_ra as usize,
      ..Default::default()
    };

    // Compute caller SP using the same formula as the walker (x86_64):
    //   caller_sp = caller_fp - (stack_size - FP_RECORD_SIZE)
    // FP_RECORD_SIZE=8 on x86_64.
    let StackSize::Known(stack_size) = callsite.stack_size else {
      panic!("fixture callsites should have a known stack_size");
    };
    let caller_sp = (caller_fp as u64) - (stack_size - 8);
    // `walk_gc_roots_from_*` yields only the *base* root slots. Derived slots (if any) are updated
    // in-place by the walker after the base slot has potentially been relocated.
    let mut expected_slots: Vec<usize> = Vec::new();
    for pair in statepoint.gc_pairs() {
      let loc = &pair.base;
      match loc {
        Location::Indirect { dwarf_reg, offset, .. } => {
          assert_eq!(
            *dwarf_reg,
            X86_64_DWARF_REG_SP,
            "fixture roots must be [SP + off]"
          );
          let slot_addr = add_signed_u64(caller_sp, *offset).expect("slot addr");
          expected_slots.push(slot_addr as usize);
        }
        other => panic!("unexpected root location kind in fixture: {other:?}"),
      }
    }
    expected_slots.sort_unstable();
    expected_slots.dedup();

    let mut visited: Vec<usize> = Vec::new();
    let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();
    unsafe {
      walk_gc_roots_from_safepoint_context(&ctx, Some(bounds), &stackmaps, |slot| {
        visited.push(slot as usize);
      })
      .expect("walk");
    }

    visited.sort_unstable();
    assert_eq!(visited, expected_slots);
    assert_eq!(visited.len(), expected_slots.len());
  }

  fn align_up(v: usize, align: usize) -> usize {
    (v + (align - 1)) & !(align - 1)
  }

  unsafe fn write_u64(addr: usize, val: u64) {
    (addr as *mut u64).write_unaligned(val);
  }

  fn add_signed_u64(base: u64, offset: i32) -> Option<u64> {
    if offset >= 0 {
      base.checked_add(offset as u64)
    } else {
      base.checked_sub((-offset) as u64)
    }
  }
}

#[cfg(target_arch = "aarch64")]
mod aarch64 {
  use runtime_native::arch::SafepointContext;
  use runtime_native::stackmaps::{Location, StackSize};
  use runtime_native::stackwalk::StackBounds;
  use runtime_native::stackwalk_fp::walk_gc_roots_from_safepoint_context;
  use runtime_native::test_util::TestRuntimeGuard;
  use runtime_native::statepoints::{AARCH64_DWARF_REG_SP, StatepointRecord};
  use runtime_native::test_util::TestRuntimeGuard;
  use runtime_native::StackMaps;

  #[test]
  fn synthetic_stack_enumerates_roots_from_safepoint_context() {
    let _rt = TestRuntimeGuard::new();
    let stackmaps = StackMaps::parse(include_bytes!("fixtures/bin/statepoint_aarch64.bin"))
      .expect("parse stackmaps");
    // Pick the first callsite record (BTreeMap iteration is sorted).
    let (callsite_ra, callsite) = stackmaps.iter().next().expect("non-empty");
    let statepoint = StatepointRecord::new(callsite.record).expect("decode statepoint layout");

    // Fake stack memory.
    let mut stack = vec![0u8; 4096];
    let base = stack.as_mut_ptr() as usize;

    // Single managed frame (terminal, caller_fp->0).
    let caller_fp = align_up(base + 3072, 16);
    unsafe {
      write_u64(caller_fp + 0, 0);
      write_u64(caller_fp + 8, 0);
    }

    let ctx = SafepointContext {
      fp: caller_fp,
      ip: callsite_ra as usize,
      ..Default::default()
    };

    // Compute caller SP using the same formula as the walker (AArch64):
    //   caller_sp = caller_fp - (stack_size - FP_RECORD_SIZE)
    // FP_RECORD_SIZE=16 on AArch64 (saved FP+LR).
    let StackSize::Known(stack_size) = callsite.stack_size else {
      panic!("fixture callsites should have a known stack_size");
    };
    let caller_sp = (caller_fp as u64) - (stack_size - 16);

    // `walk_gc_roots_from_*` yields only the *base* root slots. Derived slots (if any) are updated
    // in-place by the walker after the base slot has potentially been relocated.
    let mut expected_slots: Vec<usize> = Vec::new();
    for pair in statepoint.gc_pairs() {
      let loc = &pair.base;
      match loc {
        Location::Indirect { dwarf_reg, offset, .. } => {
          assert_eq!(
            *dwarf_reg,
            AARCH64_DWARF_REG_SP,
            "fixture roots must be [SP + off]"
          );
          let slot_addr = add_signed_u64(caller_sp, *offset).expect("slot addr");
          expected_slots.push(slot_addr as usize);
        }
        other => panic!("unexpected root location kind in fixture: {other:?}"),
      }
    }
    expected_slots.sort_unstable();
    expected_slots.dedup();

    let mut visited: Vec<usize> = Vec::new();
    let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();
    unsafe {
      walk_gc_roots_from_safepoint_context(&ctx, Some(bounds), &stackmaps, |slot| {
        visited.push(slot as usize);
      })
      .expect("walk");
    }

    visited.sort_unstable();
    assert_eq!(visited, expected_slots);
    assert_eq!(visited.len(), expected_slots.len());
  }

  fn align_up(v: usize, align: usize) -> usize {
    (v + (align - 1)) & !(align - 1)
  }

  unsafe fn write_u64(addr: usize, val: u64) {
    (addr as *mut u64).write_unaligned(val);
  }

  fn add_signed_u64(base: u64, offset: i32) -> Option<u64> {
    if offset >= 0 {
      base.checked_add(offset as u64)
    } else {
      base.checked_sub((-offset) as u64)
    }
  }
}
