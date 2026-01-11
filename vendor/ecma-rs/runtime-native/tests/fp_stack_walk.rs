// The FP stack walker is architecture/ABI-specific.

#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
mod fp_stack_walk {
  use runtime_native::arch::capture_safepoint_context;
  use runtime_native::stackwalk::{StackBounds, StackWalker, ThreadContext};
  use std::hint::black_box;

  const WANT_FRAMES: usize = 16;

  #[inline(never)]
  fn make_stack(depth: usize) -> usize {
    if depth == 0 {
      // Leave headroom for the test harness frames.
      walk_fp_chain(128)
    } else {
      let frames = make_stack(depth - 1);
      // Keep the call non-tail and resist "clever" optimizations.
      black_box(depth);
      frames
    }
  }

  #[inline(never)]
  fn walk_fp_chain(max_depth: usize) -> usize {
    let ctx = capture_safepoint_context();
    let bounds = StackBounds::current_thread().expect("stack bounds");
    let thread_ctx = ThreadContext::new(ctx.sp_entry as u64, ctx.fp as u64, ctx.ip as u64);

    let mut walker = StackWalker::new(thread_ctx, bounds)
      .expect("stack walker init")
      .with_max_depth(max_depth);
    let mut frames = 0usize;
    while let Some(frame) = walker.next() {
      frame.expect("stack walker error");
      frames += 1;
    }
    frames
  }

  #[test]
  fn fp_chain_can_walk_multiple_frames() {
    let frames = make_stack(WANT_FRAMES);
    assert!(
      frames >= WANT_FRAMES,
      "expected to walk at least {WANT_FRAMES} frames via FP chain, got {frames}"
    );
  }
}

// On other platforms we don't currently assert anything about FP walking.
#[cfg(not(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))))]
#[test]
fn fp_chain_can_walk_multiple_frames() {
  // Nothing to do.
}
