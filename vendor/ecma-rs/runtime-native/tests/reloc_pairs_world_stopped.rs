use runtime_native::gc_roots::relocate_reloc_pairs_in_place;
use runtime_native::stackmaps::Location;
use runtime_native::stackmaps::StackMaps;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use runtime_native::{set_rt_gc_safepoint_hook, FrameCursor};
use stackmap_context::ThreadContext;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

static EXPECTED_FP: AtomicUsize = AtomicUsize::new(0);
static EXPECTED_SP: AtomicUsize = AtomicUsize::new(0);
static EXPECTED_IP: AtomicU64 = AtomicU64::new(0);
static EXPECTED_BASE_SLOT: AtomicUsize = AtomicUsize::new(0);
static EXPECTED_DERIVED_SLOT: AtomicUsize = AtomicUsize::new(0);
static EXPECTED_BASE_VAL: AtomicUsize = AtomicUsize::new(0);
static EXPECTED_DELTA: AtomicUsize = AtomicUsize::new(0);
static HOOK_INSTALLED_CTX: AtomicBool = AtomicBool::new(false);

extern "C" fn install_synthetic_safepoint_context(_cursor: FrameCursor) {
  let fp = EXPECTED_FP.load(Ordering::Acquire);
  let sp = EXPECTED_SP.load(Ordering::Acquire);
  let ip = EXPECTED_IP.load(Ordering::Acquire);
  if fp == 0 || sp == 0 || ip == 0 {
    return;
  }

  #[cfg(target_arch = "x86_64")]
  let sp_entry = sp.saturating_sub(core::mem::size_of::<usize>());
  #[cfg(target_arch = "aarch64")]
  let sp_entry = sp;

  threading::registry::set_current_thread_safepoint_context(
    runtime_native::arch::SafepointContext {
      sp_entry,
      sp,
      fp,
      ip: ip as usize,
    },
  );
  HOOK_INSTALLED_CTX.store(true, Ordering::Release);
}

struct SafepointHookGuard;
impl SafepointHookGuard {
  fn install() -> Self {
    HOOK_INSTALLED_CTX.store(false, Ordering::Release);
    set_rt_gc_safepoint_hook(Some(install_synthetic_safepoint_context));
    Self
  }
}
impl Drop for SafepointHookGuard {
  fn drop(&mut self) {
    set_rt_gc_safepoint_hook(None);
    HOOK_INSTALLED_CTX.store(false, Ordering::Release);
  }
}

struct UnregisterOnDrop;
impl Drop for UnregisterOnDrop {
  fn drop(&mut self) {
    threading::unregister_current_thread();
  }
}

#[test]
fn reloc_pairs_world_stopped_enumerates_and_updates_non_stackmap_roots() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);
  let _unreg = UnregisterOnDrop;

  // Root sources that are not described by stackmaps:
  // - per-thread handle stack (`roots::Root<T>`)
  // - global root registry (`rt_gc_register_root_slot`)
  // - persistent handle table (`roots::PersistentHandleTable`)
  let before_a = 0x1111usize as *mut u8;
  let before_b = 0x2222usize as *mut u8;
  let before_c = 0x3333usize as *mut u8;

  let root_a = runtime_native::roots::Root::<u8>::new(before_a);

  let mut slot_b = before_b;
  let handle_b = runtime_native::rt_gc_register_root_slot(&mut slot_b as *mut *mut u8);
  assert_ne!(handle_b, 0);

  let id_c = runtime_native::roots::global_persistent_handle_table().alloc(before_c);
  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().get(id_c),
    Some(before_c)
  );

  threading::safepoint::with_world_stopped(|epoch| {
    let mut pairs: Vec<runtime_native::gc_roots::RelocPair> = Vec::new();
    threading::safepoint::for_each_reloc_pair_world_stopped(epoch, |pair| {
      pairs.push(pair);
    })
    .expect("reloc-pair enumeration should succeed");

    // Ensure our roots are present and appear as base roots (base_slot == derived_slot).
    let mut ctx = ThreadContext::default();
    let mut values: Vec<usize> = Vec::new();
    for pair in &pairs {
      assert_eq!(
        pair.base_slot, pair.derived_slot,
        "non-stackmap roots must be reported as (slot, slot) pairs"
      );
      values.push(pair.base_slot.read_u64(&ctx) as usize);
    }
    assert!(
      values.contains(&(before_a as usize)),
      "missing handle-stack root"
    );
    assert!(
      values.contains(&(before_b as usize)),
      "missing global root-registry root"
    );
    assert!(
      values.contains(&(before_c as usize)),
      "missing persistent-handle-table root"
    );

    // Simulate relocation: add a constant offset to all non-null pointers.
    relocate_reloc_pairs_in_place(&mut ctx, pairs, |old| old + 0x1000);
  });

  assert_eq!(root_a.get(), (before_a as usize + 0x1000) as *mut u8);
  assert_eq!(slot_b, (before_b as usize + 0x1000) as *mut u8);
  assert_eq!(
    runtime_native::roots::global_persistent_handle_table().get(id_c),
    Some((before_c as usize + 0x1000) as *mut u8)
  );

  // Cleanup.
  drop(root_a);
  runtime_native::rt_gc_unregister_root_slot(handle_b);
  assert!(runtime_native::roots::global_persistent_handle_table().free(id_c));

  // No need to drop the dummy pointers: they are opaque addresses.
}

#[test]
fn reloc_pairs_world_stopped_includes_stackmap_derived_pairs() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);
  let _unreg = UnregisterOnDrop;

  // Ensure we uninstall the safepoint hook even if the test panics.
  let _hook_guard = SafepointHookGuard::install();

  let ready = Arc::new(Barrier::new(2));
  let stop = Arc::new(AtomicBool::new(false));
  // Ensure the worker thread sees the stop flag even if the test panics.
  struct StopOnDrop(Arc<AtomicBool>);
  impl Drop for StopOnDrop {
    fn drop(&mut self) {
      self.0.store(true, Ordering::Release);
    }
  }
  let _stop_guard = StopOnDrop(stop.clone());

  std::thread::scope(|scope| {
    let ready_worker = ready.clone();
    let stop_worker = stop.clone();
    scope.spawn(move || {
      threading::register_current_thread(ThreadKind::Worker);
      let _unreg = UnregisterOnDrop;

      // Keep a synthetic "managed" stack frame in a stable stack allocation. We will override this
      // thread's published safepoint context to point at this frame so stackmap walking can run in
      // a deterministic way in tests.
      let mut fake_stack = [0u8; 4096];
      let base = fake_stack.as_mut_ptr() as usize;
      let limit = base + fake_stack.len();

      let stackmaps = StackMaps::parse(statepoint_base_derived_fixture()).expect("parse stackmaps");
      let (callsite_ip, callsite) = stackmaps
        .iter()
        .next()
        .expect("fixture must contain a callsite");
      let sp_record = runtime_native::statepoints::StatepointRecord::new(callsite.record)
        .expect("decode statepoint layout");
      let pair = sp_record
        .gc_pairs()
        .iter()
        .find(|p| p.base != p.derived)
        .expect("fixture must contain at least one derived-pointer gc pair");

      // Choose a synthetic callsite SP/FP within our stack allocation. The actual numeric values
      // do not matter (we never execute code at `callsite_ip`); they must simply be within this
      // thread's stack bounds and satisfy the FP walker's alignment checks.
      let fp = align_up(base + 2048, 16);
      let sp = align_up(base + 1024, 16);
      assert!(fp + 16 <= limit, "fake stack too small for frame record");
      assert!(sp + 16 <= limit, "fake stack too small for callsite sp");

      // Compute the base/derived *slot addresses* the stackmap will evaluate to.
      let base_slot = eval_indirect_slot(fp, sp, callsite_ip, &pair.base);
      let derived_slot = eval_indirect_slot(fp, sp, callsite_ip, &pair.derived);
      assert_ne!(
        base_slot, derived_slot,
        "expected derived slot to differ from base slot"
      );
      assert!(
        base_slot >= base && base_slot + 8 <= limit,
        "base slot address not within fake_stack allocation"
      );
      assert!(
        derived_slot >= base && derived_slot + 8 <= limit,
        "derived slot address not within fake_stack allocation"
      );

      // Ensure any older-frame walk terminates cleanly after this synthetic frame.
      unsafe {
        write_u64(fp + 0, 0);
        write_u64(fp + 8, 0);
      }

      // Populate slot *values* so relocation can preserve the interior pointer offset.
      let base_val = 0x1000usize;
      let delta = 0x20usize;
      unsafe {
        write_u64(base_slot, base_val as u64);
        write_u64(derived_slot, (base_val + delta) as u64);
      }

      EXPECTED_FP.store(fp, Ordering::Release);
      EXPECTED_SP.store(sp, Ordering::Release);
      EXPECTED_IP.store(callsite_ip, Ordering::Release);
      EXPECTED_BASE_SLOT.store(base_slot, Ordering::Release);
      EXPECTED_DERIVED_SLOT.store(derived_slot, Ordering::Release);
      EXPECTED_BASE_VAL.store(base_val, Ordering::Release);
      EXPECTED_DELTA.store(delta, Ordering::Release);

      // Signal readiness (the main thread can now request STW).
      ready_worker.wait();

      while !stop_worker.load(Ordering::Acquire) {
        // This uses the runtime safepoint helper that captures the caller's frame cursor and
        // supports the `set_rt_gc_safepoint_hook` test hook.
        runtime_native::rt_gc_safepoint();
        std::thread::yield_now();
      }
    });

    // Wait until the worker thread has registered and published the synthetic stack pointers.
    ready.wait();

    let stackmaps = StackMaps::parse(statepoint_base_derived_fixture()).expect("parse stackmaps");

    threading::safepoint::with_world_stopped(|epoch| {
      // Ensure the worker thread had a chance to run the safepoint hook and overwrite its published
      // safepoint context before we start stackmap-based enumeration.
      let start = Instant::now();
      while !HOOK_INSTALLED_CTX.load(Ordering::Acquire) {
        if start.elapsed() > Duration::from_secs(2) {
          panic!("timed out waiting for safepoint hook to install synthetic context");
        }
        std::thread::yield_now();
      }

      let mut pairs: Vec<runtime_native::gc_roots::RelocPair> = Vec::new();
      threading::safepoint::for_each_reloc_pair_world_stopped_with_stackmaps(
        epoch,
        Some(&stackmaps),
        |pair| pairs.push(pair),
      )
      .expect("reloc-pair enumeration should succeed");

      let expected_base = EXPECTED_BASE_SLOT.load(Ordering::Acquire);
      let expected_derived = EXPECTED_DERIVED_SLOT.load(Ordering::Acquire);
      let derived_pair = pairs
        .iter()
        .copied()
        .find(|pair| {
          matches!(pair.base_slot, runtime_native::statepoints::RootSlot::StackAddr(p) if p as usize == expected_base)
            && matches!(pair.derived_slot, runtime_native::statepoints::RootSlot::StackAddr(p) if p as usize == expected_derived)
        })
        .expect(&format!(
          "missing derived stackmap reloc pair (base={expected_base:#x}, derived={expected_derived:#x})"
        ));

      // Verify relocation preserves the interior-pointer delta for the derived slot.
      let mut ctx = ThreadContext::default();
      relocate_reloc_pairs_in_place(&mut ctx, [derived_pair], |old| old + 0x1000);

      let base_after = unsafe { read_u64(expected_base) };
      let derived_after = unsafe { read_u64(expected_derived) };
      let base_before = EXPECTED_BASE_VAL.load(Ordering::Acquire) as u64;
      let delta = EXPECTED_DELTA.load(Ordering::Acquire) as u64;
      assert_eq!(base_after, base_before + 0x1000);
      assert_eq!(derived_after, (base_before + 0x1000) + delta);
    });

    stop.store(true, Ordering::Release);
  });
}

fn statepoint_base_derived_fixture() -> &'static [u8] {
  #[cfg(target_arch = "x86_64")]
  {
    include_bytes!("fixtures/bin/statepoint_base_derived_x86_64.bin")
  }
  #[cfg(target_arch = "aarch64")]
  {
    include_bytes!("fixtures/bin/statepoint_base_derived_aarch64.bin")
  }
}

fn align_up(addr: usize, align: usize) -> usize {
  debug_assert!(align.is_power_of_two());
  (addr + (align - 1)) & !(align - 1)
}

unsafe fn write_u64(addr: usize, val: u64) {
  (addr as *mut u64).write_unaligned(val);
}

unsafe fn read_u64(addr: usize) -> u64 {
  (addr as *const u64).read_unaligned()
}

fn add_signed(base: usize, offset: i32) -> usize {
  if offset >= 0 {
    base.checked_add(offset as usize).expect("offset overflow")
  } else {
    base
      .checked_sub((-offset) as usize)
      .expect("offset underflow")
  }
}

fn eval_indirect_slot(caller_fp: usize, caller_sp: usize, caller_ra: u64, loc: &Location) -> usize {
  match loc {
    Location::Indirect {
      dwarf_reg, offset, ..
    } => {
      let dwarf_reg = *dwarf_reg;
      let offset = *offset;

      #[cfg(target_arch = "x86_64")]
      let base = match dwarf_reg {
        runtime_native::statepoints::X86_64_DWARF_REG_SP => caller_sp,
        runtime_native::statepoints::X86_64_DWARF_REG_FP => caller_fp,
        other => {
          panic!("unsupported base register dwarf_reg={other} for statepoint {caller_ra:#x}")
        }
      };
      #[cfg(target_arch = "aarch64")]
      let base = match dwarf_reg {
        runtime_native::statepoints::AARCH64_DWARF_REG_SP => caller_sp,
        runtime_native::statepoints::AARCH64_DWARF_REG_FP => caller_fp,
        other => {
          panic!("unsupported base register dwarf_reg={other} for statepoint {caller_ra:#x}")
        }
      };
      add_signed(base, offset)
    }
    other => panic!("unexpected location kind in fixture: {other:?}"),
  }
}
