#[cfg(target_arch = "x86_64")]
mod x86_64 {
  use runtime_native::arch::SafepointContext;
  use runtime_native::stackmaps::Location;
  use runtime_native::statepoints::{StatepointRecord, X86_64_DWARF_REG_SP};
  use runtime_native::test_util::TestRuntimeGuard;
  use runtime_native::threading;
  use runtime_native::threading::registry;
  use runtime_native::StackMaps;

  #[test]
  fn stw_root_enumeration_includes_coordinator_stack_roots() {
    let _rt = TestRuntimeGuard::new();

    threading::register_current_thread(threading::ThreadKind::Main);

    threading::safepoint::with_world_stopped(|stop_epoch| {
      let stackmaps =
        StackMaps::parse(include_bytes!("fixtures/bin/statepoint_x86_64.bin")).expect("parse stackmaps");

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

      registry::set_current_thread_safepoint_context(SafepointContext {
        fp: caller_fp,
        ip: callsite_ra as usize,
        ..Default::default()
      });
      registry::set_current_thread_safepoint_epoch_observed(stop_epoch);

      // Compute caller SP using the same formula as the walker (x86_64):
      //   caller_sp = caller_fp - (stack_size - FP_RECORD_SIZE)
      // FP_RECORD_SIZE=8 on x86_64.
      let caller_sp = (caller_fp as u64) - (callsite.stack_size - 8);

      let mut expected_slots: Vec<usize> = Vec::new();
      for pair in statepoint.gc_pairs() {
        for loc in [&pair.base, &pair.derived] {
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
  use runtime_native::statepoints::{AARCH64_DWARF_REG_SP, StatepointRecord};
  use runtime_native::test_util::TestRuntimeGuard;
  use runtime_native::threading;
  use runtime_native::threading::registry;
  use runtime_native::StackMaps;

  #[test]
  fn stw_root_enumeration_includes_coordinator_stack_roots() {
    let _rt = TestRuntimeGuard::new();

    threading::register_current_thread(threading::ThreadKind::Main);

    threading::safepoint::with_world_stopped(|stop_epoch| {
      let stackmaps =
        StackMaps::parse(include_bytes!("fixtures/bin/statepoint_aarch64.bin")).expect("parse stackmaps");

      let (callsite_ra, callsite) = stackmaps.iter().next().expect("non-empty");
      let statepoint = StatepointRecord::new(callsite.record).expect("decode statepoint layout");

      let mut stack = [0u8; 4096];
      let base = stack.as_mut_ptr() as usize;

      let caller_fp = align_up(base + 3072, 16);
      unsafe {
        write_u64(caller_fp + 0, 0);
        write_u64(caller_fp + 8, 0);
      }

      registry::set_current_thread_safepoint_context(SafepointContext {
        fp: caller_fp,
        ip: callsite_ra as usize,
        ..Default::default()
      });
      registry::set_current_thread_safepoint_epoch_observed(stop_epoch);

      // Compute caller SP using the same formula as the walker (AArch64):
      //   caller_sp = caller_fp - (stack_size - FP_RECORD_SIZE)
      // FP_RECORD_SIZE=16 on AArch64 (saved FP+LR).
      let caller_sp = (caller_fp as u64) - (callsite.stack_size - 16);

      let mut expected_slots: Vec<usize> = Vec::new();
      for pair in statepoint.gc_pairs() {
        for loc in [&pair.base, &pair.derived] {
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
