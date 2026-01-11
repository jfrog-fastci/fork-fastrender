use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use runtime_native::test_util::TestRuntimeGuard;

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn current_frame_pointer() -> usize {
  let fp: usize;
  // Safety: Reading the current frame pointer register.
  unsafe {
    core::arch::asm!("mov {0}, rbp", out(reg) fp, options(nomem, nostack, preserves_flags));
  }
  fp
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn current_frame_pointer() -> usize {
  let fp: usize;
  // Safety: Reading the current frame pointer register.
  unsafe {
    core::arch::asm!("mov {0}, x29", out(reg) fp, options(nomem, nostack, preserves_flags));
  }
  fp
}

#[test]
fn capture_safepoint_context_points_at_live_caller_frame() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  #[inline(never)]
  fn f() {
    let _guard = threading::enter_gc_safe_region();

    let ctx = threading::registry::current_thread_state()
      .unwrap()
      .safepoint_context()
      .expect("enter_gc_safe_region should publish a SafepointContext");

    // `arch::capture_safepoint_context` is invoked *inside* `enter_gc_safe_region`, so the
    // published context must refer to this function's frame, not to the runtime helper.
    assert_eq!(
      ctx.fp,
      current_frame_pointer(),
      "SafepointContext.fp should refer to the caller frame that remains live while NativeSafe",
    );

    // Keep `_guard` alive until after the assertion so the thread is still in NativeSafe.
    drop(_guard);
  }

  f();

  threading::unregister_current_thread();
}
