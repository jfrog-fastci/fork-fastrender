#[cfg(target_arch = "x86_64")]
mod x86_64 {
  use runtime_native::arch::SafepointContext;
  use runtime_native::stackmaps::Location;
  use runtime_native::statepoints::{RootSlot, StatepointRecord, X86_64_DWARF_REG_SP};
  use runtime_native::test_util::TestRuntimeGuard;
  use runtime_native::threading;
  use runtime_native::threading::registry;
  use runtime_native::StackMaps;

  #[test]
  fn stw_reloc_pair_enumeration_includes_coordinator_stack_pairs() {
    let _rt = TestRuntimeGuard::new();

    threading::register_current_thread(threading::ThreadKind::Main);

    threading::safepoint::with_world_stopped(|stop_epoch| {
      let stackmaps = StackMaps::parse(include_bytes!("fixtures/bin/statepoint_base_derived_x86_64.bin"))
        .expect("parse stackmaps");

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

      let mut expected_pairs: Vec<(usize, usize)> = Vec::new();
      for pair in statepoint.gc_pairs() {
        let base_addr = slot_addr(caller_sp as u64, &pair.base);
        let derived_addr = slot_addr(caller_sp as u64, &pair.derived);
        expected_pairs.push((base_addr, derived_addr));
      }
      expected_pairs.sort_unstable();
      expected_pairs.dedup();
      assert!(
        expected_pairs.iter().any(|(b, d)| b != d),
        "fixture should contain at least one base!=derived stack slot pair"
      );

      let mut visited: Vec<(usize, usize)> = Vec::new();
      threading::safepoint::for_each_reloc_pair_world_stopped_with_stackmaps(
        stop_epoch,
        Some(&stackmaps),
        |pair| {
          let (RootSlot::StackAddr(base_slot), RootSlot::StackAddr(derived_slot)) =
            (pair.base_slot, pair.derived_slot)
          else {
            return;
          };
          visited.push((base_slot as usize, derived_slot as usize));
        },
      )
      .expect("enumerate relocation pairs");
      visited.sort_unstable();
      visited.dedup();

      let stack_range = base..(base + stack.len());
      let visited_stack: Vec<(usize, usize)> = visited
        .into_iter()
        .filter(|(b, d)| stack_range.contains(b) && stack_range.contains(d))
        .collect();

      assert!(
        !visited_stack.is_empty(),
        "expected at least one coordinator stack relocation pair"
      );
      assert!(
        visited_stack.iter().any(|(b, d)| b != d),
        "expected at least one derived-pointer (base!=derived) relocation pair"
      );
      assert_eq!(visited_stack, expected_pairs);
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

  fn slot_addr(sp: u64, loc: &Location) -> usize {
    match loc {
      Location::Indirect { dwarf_reg, offset, .. } => {
        assert_eq!(
          *dwarf_reg,
          X86_64_DWARF_REG_SP,
          "fixture roots must be [SP + off]"
        );
        add_signed_u64(sp, *offset).expect("slot addr") as usize
      }
      other => panic!("unexpected root location kind in fixture: {other:?}"),
    }
  }
}

#[cfg(target_arch = "aarch64")]
mod aarch64 {
  use runtime_native::arch::SafepointContext;
  use runtime_native::stackmaps::Location;
  use runtime_native::statepoints::{AARCH64_DWARF_REG_SP, RootSlot, StatepointRecord};
  use runtime_native::test_util::TestRuntimeGuard;
  use runtime_native::threading;
  use runtime_native::threading::registry;
  use runtime_native::StackMaps;

  #[test]
  fn stw_reloc_pair_enumeration_includes_coordinator_stack_pairs() {
    let _rt = TestRuntimeGuard::new();

    threading::register_current_thread(threading::ThreadKind::Main);

    threading::safepoint::with_world_stopped(|stop_epoch| {
      let stackmaps = StackMaps::parse(include_bytes!("fixtures/bin/statepoint_base_derived_aarch64.bin"))
        .expect("parse stackmaps");

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

      let mut expected_pairs: Vec<(usize, usize)> = Vec::new();
      for pair in statepoint.gc_pairs() {
        let base_addr = slot_addr(caller_sp as u64, &pair.base);
        let derived_addr = slot_addr(caller_sp as u64, &pair.derived);
        expected_pairs.push((base_addr, derived_addr));
      }
      expected_pairs.sort_unstable();
      expected_pairs.dedup();
      assert!(
        expected_pairs.iter().any(|(b, d)| b != d),
        "fixture should contain at least one base!=derived stack slot pair"
      );

      let mut visited: Vec<(usize, usize)> = Vec::new();
      threading::safepoint::for_each_reloc_pair_world_stopped_with_stackmaps(
        stop_epoch,
        Some(&stackmaps),
        |pair| {
          let (RootSlot::StackAddr(base_slot), RootSlot::StackAddr(derived_slot)) =
            (pair.base_slot, pair.derived_slot)
          else {
            return;
          };
          visited.push((base_slot as usize, derived_slot as usize));
        },
      )
      .expect("enumerate relocation pairs");
      visited.sort_unstable();
      visited.dedup();

      let stack_range = base..(base + stack.len());
      let visited_stack: Vec<(usize, usize)> = visited
        .into_iter()
        .filter(|(b, d)| stack_range.contains(b) && stack_range.contains(d))
        .collect();

      assert!(
        !visited_stack.is_empty(),
        "expected at least one coordinator stack relocation pair"
      );
      assert!(
        visited_stack.iter().any(|(b, d)| b != d),
        "expected at least one derived-pointer (base!=derived) relocation pair"
      );
      assert_eq!(visited_stack, expected_pairs);
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

  fn slot_addr(sp: u64, loc: &Location) -> usize {
    match loc {
      Location::Indirect { dwarf_reg, offset, .. } => {
        assert_eq!(
          *dwarf_reg,
          AARCH64_DWARF_REG_SP,
          "fixture roots must be [SP + off]"
        );
        add_signed_u64(sp, *offset).expect("slot addr") as usize
      }
      other => panic!("unexpected root location kind in fixture: {other:?}"),
    }
  }
}
