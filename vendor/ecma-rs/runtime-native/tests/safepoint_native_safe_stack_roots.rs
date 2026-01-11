use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::sync::mpsc;
 
#[cfg(target_arch = "x86_64")]
mod x86_64 {
  use super::*;
  use runtime_native::arch::SafepointContext;
  use runtime_native::stackmaps::Location;
  use runtime_native::stackwalk_fp::ensure_stackwalk_scratch_capacity;
  use runtime_native::statepoints::{StatepointRecord, X86_64_DWARF_REG_FP, X86_64_DWARF_REG_SP};
  use runtime_native::StackMaps;
 
  #[test]
  fn stw_scans_native_safe_threads_using_published_safepoint_context() {
    let _rt = TestRuntimeGuard::new();
 
    threading::register_current_thread(ThreadKind::Main);
 
    let (tx_ready, rx_ready) = mpsc::channel::<(usize, usize, Vec<usize>)>();
    let (tx_done, rx_done) = mpsc::channel::<()>();
 
    let worker = std::thread::spawn(move || {
      threading::register_current_thread(ThreadKind::Worker);
      let gc_safe = threading::enter_gc_safe_region();
 
      // Construct a synthetic "managed" stack region (backed by the thread's real stack memory so
      // it falls within the published stack bounds).
      let stackmaps =
        StackMaps::parse(include_bytes!("fixtures/bin/statepoint_x86_64.bin")).expect("parse stackmaps");
 
      // Pick the first callsite record (BTreeMap iteration is sorted).
      let (callsite_ra, callsite) = stackmaps.iter().next().expect("non-empty");
      let statepoint = StatepointRecord::new(callsite.record).expect("decode statepoint layout");
 
      // Fake stack memory (on-stack so stack bounds checks succeed).
      let mut stack = [0u8; 4096];
      let base = stack.as_mut_ptr() as usize;

      // Synthetic runtime frame (lower address) calling into a managed frame (higher address).
      //
      // The stackmap record is keyed by the return address back into the managed caller
      // (`callsite_ra`), and `Indirect [SP + off]` locations are based on the *managed caller's*
      // stack pointer value at that return address.
      let runtime_fp = align_up(base + 2048, 16);
      let managed_fp = align_up(base + 3072, 16);
      unsafe {
        // Runtime frame record:
        //   [FP + 0] = saved FP (managed caller frame pointer)
        //   [FP + 8] = return address into managed code (stackmap PC)
        write_u64(runtime_fp + 0, managed_fp as u64);
        write_u64(runtime_fp + 8, callsite_ra);

        // Managed frame record: terminate the chain.
        write_u64(managed_fp + 0, 0);
        write_u64(managed_fp + 8, 0);
      }

      // Under the forced-frame-pointer ABI contract on x86_64 SysV:
      //   caller_sp_callsite = callee_fp + 16
      let caller_sp = runtime_fp as u64 + 16;
 
      // `walk_gc_roots_from_*` yields only the *base* root slots. Derived slots (if any) are updated
      // in-place by the walker after the base slot has potentially been relocated.
      let mut expected_slots: Vec<usize> = Vec::new();
      for pair in statepoint.gc_pairs() {
        let loc = &pair.base;
        match loc {
          Location::Indirect { dwarf_reg, offset, .. } => {
            let base = if *dwarf_reg == X86_64_DWARF_REG_SP {
              caller_sp
            } else if *dwarf_reg == X86_64_DWARF_REG_FP {
              managed_fp as u64
            } else {
              panic!(
                "unexpected dwarf_reg={dwarf_reg} (expected SP={X86_64_DWARF_REG_SP} or FP={X86_64_DWARF_REG_FP})"
              );
            };
            let slot_addr = add_signed_u64(base, *offset).expect("slot addr");
            expected_slots.push(slot_addr as usize);
          }
          other => panic!("unexpected root location kind in fixture: {other:?}"),
        }
      }
      expected_slots.sort_unstable();
      expected_slots.dedup();
      assert!(!expected_slots.is_empty(), "fixture should contain at least one GC root slot");
 
      // Publish the synthetic safepoint context: this thread is NativeSafe, so it may not observe
      // the stop epoch, but the GC should still scan from this context.
      //
      // Provide the stackmap-semantics SP base explicitly: the runtime capture path publishes
      // `sp = runtime_fp + 16` and `sp_entry = sp - 8` for x86_64.
      let sp_callsite = runtime_fp + 16;
      let sp_entry = sp_callsite - 8;
      let ctx = SafepointContext {
        sp_entry,
        sp: sp_callsite,
        fp: managed_fp,
        ip: callsite_ra as usize,
        regs: std::ptr::null_mut(),
      };
      runtime_native::test_util::set_current_thread_safepoint_context_for_tests(ctx);
 
      tx_ready
        .send((base, base + stack.len(), expected_slots))
        .expect("send ready");
 
      // Stay blocked + NativeSafe until the test completes.
      rx_done.recv().expect("recv done");
      // Keep the synthetic stack region alive across the blocking call above: we publish its
      // addresses to the GC via `SafepointContext`, so the compiler must not reuse its stack slot
      // while we're blocked.
      std::hint::black_box(&stack);
 
      drop(gc_safe);
      threading::unregister_current_thread();
    });
 
    let (stack_lo, stack_hi, expected_slots) = rx_ready.recv().expect("recv ready");
    let stackmaps =
      StackMaps::parse(include_bytes!("fixtures/bin/statepoint_x86_64.bin")).expect("parse stackmaps");
    ensure_stackwalk_scratch_capacity(stackmaps.max_gc_pairs_per_frame());

    threading::safepoint::with_world_stopped(|stop_epoch| {
      let mut visited: Vec<usize> = Vec::new();
      threading::safepoint::for_each_root_slot_world_stopped_with_stackmaps(
        stop_epoch,
        Some(&stackmaps),
        |slot| visited.push(slot as usize),
      )
      .expect("enumerate roots");
 
      let mut visited_synthetic: Vec<usize> = visited
        .into_iter()
        .filter(|&slot| (stack_lo..stack_hi).contains(&slot))
        .collect();
      visited_synthetic.sort_unstable();
 
      assert_eq!(visited_synthetic, expected_slots);
    });
 
    // Allow the worker to exit after the STW pause has ended (avoid deadlock on GC-safe guard drop).
    tx_done.send(()).expect("send done");
    worker.join().unwrap();
 
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
  use super::*;
  use runtime_native::arch::SafepointContext;
  use runtime_native::stackmaps::Location;
  use runtime_native::stackwalk_fp::ensure_stackwalk_scratch_capacity;
  use runtime_native::statepoints::{AARCH64_DWARF_REG_FP, AARCH64_DWARF_REG_SP, StatepointRecord};
  use runtime_native::StackMaps;
 
  #[test]
  fn stw_scans_native_safe_threads_using_published_safepoint_context() {
    let _rt = TestRuntimeGuard::new();
 
    threading::register_current_thread(ThreadKind::Main);
 
    let (tx_ready, rx_ready) = mpsc::channel::<(usize, usize, Vec<usize>)>();
    let (tx_done, rx_done) = mpsc::channel::<()>();
 
    let worker = std::thread::spawn(move || {
      threading::register_current_thread(ThreadKind::Worker);
      let gc_safe = threading::enter_gc_safe_region();
 
      // Construct a synthetic "managed" stack region (backed by the thread's real stack memory so
      // it falls within the published stack bounds).
      let stackmaps =
        StackMaps::parse(include_bytes!("fixtures/bin/statepoint_aarch64.bin")).expect("parse stackmaps");
 
      // Pick the first callsite record (BTreeMap iteration is sorted).
      let (callsite_ra, callsite) = stackmaps.iter().next().expect("non-empty");
      let statepoint = StatepointRecord::new(callsite.record).expect("decode statepoint layout");
 
      // Fake stack memory (on-stack so stack bounds checks succeed).
      let mut stack = [0u8; 4096];
      let base = stack.as_mut_ptr() as usize;

      // Synthetic runtime frame (lower address) calling into a managed frame (higher address).
      //
      // The stackmap record is keyed by the return address back into the managed caller
      // (`callsite_ra`), and `Indirect [SP + off]` locations are based on the *managed caller's*
      // stack pointer value at that return address.
      let runtime_fp = align_up(base + 2048, 16);
      let managed_fp = align_up(base + 3072, 16);
      unsafe {
        // Runtime frame record:
        //   [FP + 0] = saved FP (managed caller frame pointer)
        //   [FP + 8] = saved LR (return address into managed code; stackmap PC)
        write_u64(runtime_fp + 0, managed_fp as u64);
        write_u64(runtime_fp + 8, callsite_ra);

        // Managed frame record: terminate the chain.
        write_u64(managed_fp + 0, 0);
        write_u64(managed_fp + 8, 0);
      }

      // Under the forced-frame-pointer ABI contract on AArch64:
      //   caller_sp_callsite = callee_fp + 16
      let caller_sp = runtime_fp as u64 + 16;
 
      // `walk_gc_roots_from_*` yields only the *base* root slots. Derived slots (if any) are updated
      // in-place by the walker after the base slot has potentially been relocated.
      let mut expected_slots: Vec<usize> = Vec::new();
      for pair in statepoint.gc_pairs() {
        let loc = &pair.base;
        match loc {
          Location::Indirect { dwarf_reg, offset, .. } => {
            let base = if *dwarf_reg == AARCH64_DWARF_REG_SP {
              caller_sp
            } else if *dwarf_reg == AARCH64_DWARF_REG_FP {
              managed_fp as u64
            } else {
              panic!(
                "unexpected dwarf_reg={dwarf_reg} (expected SP={AARCH64_DWARF_REG_SP} or FP={AARCH64_DWARF_REG_FP})"
              );
            };
            let slot_addr = add_signed_u64(base, *offset).expect("slot addr");
            expected_slots.push(slot_addr as usize);
          }
          other => panic!("unexpected root location kind in fixture: {other:?}"),
        }
      }
      expected_slots.sort_unstable();
      expected_slots.dedup();
      assert!(!expected_slots.is_empty(), "fixture should contain at least one GC root slot");
 
      // Publish the synthetic safepoint context: this thread is NativeSafe, so it may not observe
      // the stop epoch, but the GC should still scan from this context.
      //
      // Provide the stackmap-semantics SP base explicitly: AArch64 `bl` does not push a return
      // address onto the stack, so `sp_entry == sp` at the callsite.
      let sp_callsite = runtime_fp + 16;
      let ctx = SafepointContext {
        sp_entry: sp_callsite,
        sp: sp_callsite,
        fp: managed_fp,
        ip: callsite_ra as usize,
        regs: std::ptr::null_mut(),
      };
      runtime_native::test_util::set_current_thread_safepoint_context_for_tests(ctx);
 
      tx_ready
        .send((base, base + stack.len(), expected_slots))
        .expect("send ready");
 
      // Stay blocked + NativeSafe until the test completes.
      rx_done.recv().expect("recv done");
      // Keep the synthetic stack region alive across the blocking call above: we publish its
      // addresses to the GC via `SafepointContext`, so the compiler must not reuse its stack slot
      // while we're blocked.
      std::hint::black_box(&stack);
 
      drop(gc_safe);
      threading::unregister_current_thread();
    });
 
    let (stack_lo, stack_hi, expected_slots) = rx_ready.recv().expect("recv ready");
    let stackmaps =
      StackMaps::parse(include_bytes!("fixtures/bin/statepoint_aarch64.bin")).expect("parse stackmaps");
    ensure_stackwalk_scratch_capacity(stackmaps.max_gc_pairs_per_frame());

    threading::safepoint::with_world_stopped(|stop_epoch| {
      let mut visited: Vec<usize> = Vec::new();
      threading::safepoint::for_each_root_slot_world_stopped_with_stackmaps(
        stop_epoch,
        Some(&stackmaps),
        |slot| visited.push(slot as usize),
      )
      .expect("enumerate roots");
 
      let mut visited_synthetic: Vec<usize> = visited
        .into_iter()
        .filter(|&slot| (stack_lo..stack_hi).contains(&slot))
        .collect();
      visited_synthetic.sort_unstable();
 
      assert_eq!(visited_synthetic, expected_slots);
    });
 
    // Allow the worker to exit after the STW pause has ended (avoid deadlock on GC-safe guard drop).
    tx_done.send(()).expect("send done");
    worker.join().unwrap();
 
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
