#[cfg(target_arch = "x86_64")]
mod x86_64 {
  use runtime_native::arch::SafepointContext;
  use runtime_native::stackmaps::Location;
  use runtime_native::stackwalk_fp::ensure_stackwalk_scratch_capacity;
  use runtime_native::statepoints::{StatepointRecord, X86_64_DWARF_REG_FP, X86_64_DWARF_REG_SP};
  use runtime_native::test_util::TestRuntimeGuard;
  use runtime_native::threading;
  use runtime_native::threading::registry;
  use runtime_native::StackMaps;

  #[test]
  fn stw_root_enumeration_includes_coordinator_stack_roots() {
    let _rt = TestRuntimeGuard::new();

    threading::register_current_thread(threading::ThreadKind::Main);

    let stackmaps =
      StackMaps::parse(include_bytes!("fixtures/bin/statepoint_x86_64.bin")).expect("parse stackmaps");
    ensure_stackwalk_scratch_capacity(stackmaps.max_gc_pairs_per_frame());

    threading::safepoint::with_world_stopped(|stop_epoch| {
      // Pick the first callsite record (BTreeMap iteration is sorted).
      let (callsite_ra, callsite) = stackmaps.iter().next().expect("non-empty");
      let statepoint = StatepointRecord::new(callsite.record).expect("decode statepoint layout");

      let mut stack = [0u8; 4096];
      let base = stack.as_mut_ptr() as usize;

      // Single managed frame (terminal, caller_fp->0).
      let caller_fp = align_up(base + 3072, 16);
      unsafe {
        write_u64(caller_fp + 0, 0);
        write_u64(caller_fp + 8, 0);
      }

      // Provide a stackmap-semantics (post-call) SP for the coordinator's top frame.
      let caller_sp = align_up(base + 2048, 16);
      let sp_entry = caller_sp - 8;

      registry::set_current_thread_safepoint_context(SafepointContext {
        sp_entry,
        sp: caller_sp,
        fp: caller_fp,
        ip: callsite_ra as usize,
        ..Default::default()
      });
      registry::set_current_thread_safepoint_epoch_observed(stop_epoch);

      let mut expected_slots: Vec<usize> = Vec::new();
      for pair in statepoint.gc_pairs() {
        let base_loc = &pair.base;
        let derived_loc = &pair.derived;

        let (Location::Indirect { dwarf_reg: base_reg, offset: base_off, .. }, Location::Indirect { dwarf_reg: derived_reg, offset: derived_off, .. }) =
          (base_loc, derived_loc)
        else {
          panic!("unexpected root location kind in fixture: base={base_loc:?} derived={derived_loc:?}");
        };
        assert!(
          *base_reg == X86_64_DWARF_REG_SP || *base_reg == X86_64_DWARF_REG_FP,
          "expected SP/FP-relative root slot, got dwarf_reg={base_reg}"
        );
        assert!(
          *derived_reg == X86_64_DWARF_REG_SP || *derived_reg == X86_64_DWARF_REG_FP,
          "expected SP/FP-relative root slot, got dwarf_reg={derived_reg}"
        );

        let base_base = if *base_reg == X86_64_DWARF_REG_SP {
          caller_sp as u64
        } else {
          caller_fp as u64
        };
        let derived_base = if *derived_reg == X86_64_DWARF_REG_SP {
          caller_sp as u64
        } else {
          caller_fp as u64
        };

        let base_addr = add_signed_u64(base_base, *base_off).expect("slot addr");
        let derived_addr = add_signed_u64(derived_base, *derived_off).expect("slot addr");

        // Seed slot values so the walker can compute derived fixups.
        unsafe {
          write_u64(base_addr as usize, 0x1111_2222_3333_4444);
          write_u64(derived_addr as usize, 0x1111_2222_3333_4444);
        }

        expected_slots.push(base_addr as usize);
      }
      expected_slots.sort_unstable();
      expected_slots.dedup();

      let mut visited: Vec<usize> = Vec::new();
      threading::safepoint::for_each_root_slot_world_stopped_with_stackmaps(
        stop_epoch,
        Some(&stackmaps),
        |slot| visited.push(slot as usize),
      )
      .expect("enumerate roots");
      visited.sort_unstable();
      visited.dedup();

      let stack_range = base..(base + stack.len());
      let visited_stack: Vec<usize> = visited
        .into_iter()
        .filter(|&addr| stack_range.contains(&addr))
        .collect();

      assert!(!visited_stack.is_empty(), "expected at least one coordinator stack root slot");
      assert_eq!(visited_stack, expected_slots);
    });

    threading::unregister_current_thread();
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
  use runtime_native::statepoints::{AARCH64_DWARF_REG_FP, AARCH64_DWARF_REG_SP, StatepointRecord};
  use runtime_native::test_util::TestRuntimeGuard;
  use runtime_native::threading;
  use runtime_native::threading::registry;
  use runtime_native::stackwalk_fp::ensure_stackwalk_scratch_capacity;
  use runtime_native::StackMaps;

  #[test]
  fn stw_root_enumeration_includes_coordinator_stack_roots() {
    let _rt = TestRuntimeGuard::new();

    threading::register_current_thread(threading::ThreadKind::Main);

    let stackmaps =
      StackMaps::parse(include_bytes!("fixtures/bin/statepoint_aarch64.bin")).expect("parse stackmaps");
    ensure_stackwalk_scratch_capacity(stackmaps.max_gc_pairs_per_frame());

    threading::safepoint::with_world_stopped(|stop_epoch| {
      let (callsite_ra, callsite) = stackmaps.iter().next().expect("non-empty");
      let statepoint = StatepointRecord::new(callsite.record).expect("decode statepoint layout");

      let mut stack = [0u8; 4096];
      let base = stack.as_mut_ptr() as usize;

      let caller_fp = align_up(base + 3072, 16);
      unsafe {
        write_u64(caller_fp + 0, 0);
        write_u64(caller_fp + 8, 0);
      }

      let caller_sp = align_up(base + 2048, 16);
      let sp_entry = caller_sp;

      registry::set_current_thread_safepoint_context(SafepointContext {
        sp_entry,
        sp: caller_sp,
        fp: caller_fp,
        ip: callsite_ra as usize,
        ..Default::default()
      });
      registry::set_current_thread_safepoint_epoch_observed(stop_epoch);

      let mut expected_slots: Vec<usize> = Vec::new();
      for pair in statepoint.gc_pairs() {
        let base_loc = &pair.base;
        let derived_loc = &pair.derived;

        let (Location::Indirect { dwarf_reg: base_reg, offset: base_off, .. }, Location::Indirect { dwarf_reg: derived_reg, offset: derived_off, .. }) =
          (base_loc, derived_loc)
        else {
          panic!("unexpected root location kind in fixture: base={base_loc:?} derived={derived_loc:?}");
        };
        assert!(
          *base_reg == AARCH64_DWARF_REG_SP || *base_reg == AARCH64_DWARF_REG_FP,
          "expected SP/FP-relative root slot, got dwarf_reg={base_reg}"
        );
        assert!(
          *derived_reg == AARCH64_DWARF_REG_SP || *derived_reg == AARCH64_DWARF_REG_FP,
          "expected SP/FP-relative root slot, got dwarf_reg={derived_reg}"
        );

        let base_base = if *base_reg == AARCH64_DWARF_REG_SP {
          caller_sp as u64
        } else {
          caller_fp as u64
        };
        let derived_base = if *derived_reg == AARCH64_DWARF_REG_SP {
          caller_sp as u64
        } else {
          caller_fp as u64
        };

        let base_addr = add_signed_u64(base_base, *base_off).expect("slot addr");
        let derived_addr = add_signed_u64(derived_base, *derived_off).expect("slot addr");

        unsafe {
          write_u64(base_addr as usize, 0x1111_2222_3333_4444);
          write_u64(derived_addr as usize, 0x1111_2222_3333_4444);
        }

        expected_slots.push(base_addr as usize);
      }
      expected_slots.sort_unstable();
      expected_slots.dedup();

      let mut visited: Vec<usize> = Vec::new();
      threading::safepoint::for_each_root_slot_world_stopped_with_stackmaps(
        stop_epoch,
        Some(&stackmaps),
        |slot| visited.push(slot as usize),
      )
      .expect("enumerate roots");
      visited.sort_unstable();
      visited.dedup();

      let stack_range = base..(base + stack.len());
      let visited_stack: Vec<usize> = visited
        .into_iter()
        .filter(|&addr| stack_range.contains(&addr))
        .collect();

      assert!(!visited_stack.is_empty(), "expected at least one coordinator stack root slot");
      assert_eq!(visited_stack, expected_slots);
    });

    threading::unregister_current_thread();
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
