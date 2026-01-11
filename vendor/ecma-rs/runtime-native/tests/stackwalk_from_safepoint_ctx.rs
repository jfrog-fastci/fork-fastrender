#[cfg(target_arch = "x86_64")]
mod x86_64 {
  use runtime_native::arch::SafepointContext;
  use runtime_native::stackmaps::Location;
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
    let hi = base + stack.len();

    // Single managed frame (terminal, caller_fp->0).
    let caller_fp = align_up(base + 3072, 16);
    unsafe {
      write_u64(caller_fp + 0, 0);
      write_u64(caller_fp + 8, 0);
    }

    // Provide a stackmap-semantics (post-call) SP for the top managed frame.
    let caller_sp = align_up(base + 2048, 16);
    // x86_64 `call` pushes the return address, so callee-entry SP is 8 bytes lower.
    let sp_entry = caller_sp - 8;

    let ctx = SafepointContext {
      fp: caller_fp,
      ip: callsite_ra as usize,
      sp_entry,
      sp: caller_sp,
      ..Default::default()
    };

    // `walk_gc_roots_from_*` yields only the *base* root slots. Derived slots (if any) are updated
    // in-place by the walker after the base slot has potentially been relocated.
    let mut expected_slots: Vec<usize> = Vec::new();
    for pair in statepoint.gc_pairs() {
      let (base_off, derived_off) = match (&pair.base, &pair.derived) {
        (
          Location::Indirect {
            dwarf_reg: base_reg,
            offset: base_off,
            ..
          },
          Location::Indirect {
            dwarf_reg: derived_reg,
            offset: derived_off,
            ..
          },
        ) => {
          assert_eq!(
            *base_reg,
            X86_64_DWARF_REG_SP,
            "fixture roots must be [SP + off]"
          );
          assert_eq!(
            *derived_reg,
            X86_64_DWARF_REG_SP,
            "fixture roots must be [SP + off]"
          );
          (*base_off, *derived_off)
        }
        other => panic!("unexpected root location kind in fixture: {other:?}"),
      };

      let base_addr = add_signed_u64(caller_sp as u64, base_off).expect("slot addr");
      let derived_addr = add_signed_u64(caller_sp as u64, derived_off).expect("slot addr");

      for &addr in &[base_addr, derived_addr] {
        assert!(
          addr >= base as u64 && addr + 8 <= hi as u64,
          "fixture root slot {addr:#x} is outside synthetic stack [{base:#x}, {hi:#x})"
        );
      }

      // Seed the slots with non-null dummy pointers so the walker can compute derived fixups without
      // tripping the "force null" path.
      unsafe {
        write_u64(base_addr as usize, 0x1111_2222_3333_4444);
        write_u64(derived_addr as usize, 0x1111_2222_3333_4444);
      }

      expected_slots.push(base_addr as usize);
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
  use runtime_native::stackmaps::Location;
  use runtime_native::stackwalk::StackBounds;
  use runtime_native::stackwalk_fp::walk_gc_roots_from_safepoint_context;
  use runtime_native::test_util::TestRuntimeGuard;
  use runtime_native::statepoints::{AARCH64_DWARF_REG_SP, StatepointRecord};
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
    let hi = base + stack.len();

    // Single managed frame (terminal, caller_fp->0).
    let caller_fp = align_up(base + 3072, 16);
    unsafe {
      write_u64(caller_fp + 0, 0);
      write_u64(caller_fp + 8, 0);
    }

    // Provide a stackmap-semantics SP for the top managed frame.
    let caller_sp = align_up(base + 2048, 16);
    // AArch64 `bl` does not push a return address, so callee-entry SP equals stackmap SP.
    let sp_entry = caller_sp;

    let ctx = SafepointContext {
      fp: caller_fp,
      ip: callsite_ra as usize,
      sp_entry,
      sp: caller_sp,
      ..Default::default()
    };

    // `walk_gc_roots_from_*` yields only the *base* root slots. Derived slots (if any) are updated
    // in-place by the walker after the base slot has potentially been relocated.
    let mut expected_slots: Vec<usize> = Vec::new();
    for pair in statepoint.gc_pairs() {
      let (base_off, derived_off) = match (&pair.base, &pair.derived) {
        (
          Location::Indirect {
            dwarf_reg: base_reg,
            offset: base_off,
            ..
          },
          Location::Indirect {
            dwarf_reg: derived_reg,
            offset: derived_off,
            ..
          },
        ) => {
          assert_eq!(
            *base_reg,
            AARCH64_DWARF_REG_SP,
            "fixture roots must be [SP + off]"
          );
          assert_eq!(
            *derived_reg,
            AARCH64_DWARF_REG_SP,
            "fixture roots must be [SP + off]"
          );
          (*base_off, *derived_off)
        }
        other => panic!("unexpected root location kind in fixture: {other:?}"),
      };

      let base_addr = add_signed_u64(caller_sp as u64, base_off).expect("slot addr");
      let derived_addr = add_signed_u64(caller_sp as u64, derived_off).expect("slot addr");

      for &addr in &[base_addr, derived_addr] {
        assert!(
          addr >= base as u64 && addr + 8 <= hi as u64,
          "fixture root slot {addr:#x} is outside synthetic stack [{base:#x}, {hi:#x})"
        );
      }

      unsafe {
        write_u64(base_addr as usize, 0x1111_2222_3333_4444);
        write_u64(derived_addr as usize, 0x1111_2222_3333_4444);
      }

      expected_slots.push(base_addr as usize);
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
