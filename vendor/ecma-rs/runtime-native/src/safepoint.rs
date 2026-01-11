use crate::arch;
use crate::stackwalk::StackBounds;
use crate::threading;
use crate::WalkError;
use std::time::Duration;

/// Visit `(slot, value)` relocation pairs for GC roots reachable from a safepoint.
///
/// `top_callee_fp` must be the frame pointer of the runtime entrypoint frame
/// (`rt_gc_safepoint` / `rt_gc_collect`) at the safepoint callsite.
pub fn visit_reloc_pairs(
  top_callee_fp: u64,
  bounds: Option<StackBounds>,
  visit: &mut dyn FnMut(*mut *mut u8, *mut u8),
) -> Result<(), WalkError> {
  visit_reloc_pairs_with_bounds(top_callee_fp, bounds, visit)
}

/// Like [`visit_reloc_pairs`], but allows passing stack bounds for additional safety checks.
///
/// When available, stack bounds help catch bogus frame-pointer chains early by ensuring each frame
/// pointer stays within the stopped thread's stack range.
pub fn visit_reloc_pairs_with_bounds(
  top_callee_fp: u64,
  bounds: Option<StackBounds>,
  visit: &mut dyn FnMut(*mut *mut u8, *mut u8),
) -> Result<(), WalkError> {
  let Some(stackmaps) = crate::stackmap::try_stackmaps() else {
    return Ok(());
  };

  let bounds = bounds.or_else(|| crate::stackwalk::StackBounds::current_thread().ok());

  // Safety: stackmap-driven root enumeration inherently walks raw pointers into
  // thread stacks.
  unsafe {
    crate::walk_gc_roots_from_fp(top_callee_fp, bounds, stackmaps, |slot_addr| {
      let slot = slot_addr as *mut *mut u8;
      // Read the current pointer value in the slot so GC can relocate it.
      let value = slot.read();
      visit(slot, value);
    })
  }
}

/// Visit `(slot, value)` relocation pairs for a safepoint described by a captured [`arch::SafepointContext`].
///
/// This is the variant used when we don't have the runtime callee's frame pointer (e.g. the current
/// thread is the GC coordinator and is not stopped inside `rt_gc_safepoint`), but we *do* have a
/// previously published call-site snapshot (`fp` + return address) for the nearest managed frame.
pub fn visit_reloc_pairs_from_safepoint_context(
  ctx: &arch::SafepointContext,
  visit: &mut dyn FnMut(*mut *mut u8, *mut u8),
) -> Result<(), WalkError> {
  visit_reloc_pairs_from_safepoint_context_with_bounds(ctx, None, visit)
}

/// Like [`visit_reloc_pairs_from_safepoint_context`], but allows passing stack bounds for additional safety checks.
pub fn visit_reloc_pairs_from_safepoint_context_with_bounds(
  ctx: &arch::SafepointContext,
  bounds: Option<StackBounds>,
  visit: &mut dyn FnMut(*mut *mut u8, *mut u8),
) -> Result<(), WalkError> {
  let Some(stackmaps) = crate::stackmap::try_stackmaps() else {
    return Ok(());
  };

  let bounds = bounds.or_else(|| crate::stackwalk::StackBounds::current_thread().ok());

  // Safety: stackmap-driven root enumeration inherently walks raw pointers into thread stacks.
  unsafe {
    crate::stackwalk_fp::walk_gc_roots_from_safepoint_context(ctx, bounds, stackmaps, |slot_addr| {
      let slot = slot_addr as *mut *mut u8;
      // Read the current pointer value in the slot so GC can relocate it.
      let value = slot.read();
      visit(slot, value);
    })
  }
}

/// Cooperatively enter a safepoint at the current callsite while a stop-the-world
/// request is active.
///
/// This is used by `rt_gc_collect` when a thread attempts to initiate GC while a
/// collection is already in progress: it must still publish a safepoint context
/// and block until the world is resumed.
pub(crate) fn enter_safepoint_at_current_callsite(stop_epoch: u64) {
  // If we're inside runtime code (e.g. the allocator slow path called
  // `rt_gc_collect`) we may be several runtime frames away from the nearest
  // managed callsite that has a stackmap record. Recover that callsite cursor by
  // walking the FP chain.
  let ctx = crate::stackmap::try_stackmaps()
    .and_then(|stackmaps| crate::stackwalk::find_nearest_managed_cursor_from_here(stackmaps))
    .map(|cursor| {
      let sp_callsite = cursor.sp.unwrap_or(0);
      #[cfg(target_arch = "x86_64")]
      let sp_entry = sp_callsite.saturating_sub(crate::arch::WORD_SIZE as u64);
      #[cfg(not(target_arch = "x86_64"))]
      let sp_entry = sp_callsite;

      crate::arch::SafepointContext {
        sp_entry: sp_entry as usize,
        sp: sp_callsite as usize,
        fp: cursor.fp as usize,
        ip: cursor.pc as usize,
      }
    })
    .unwrap_or_else(arch::capture_safepoint_context);
  threading::registry::set_current_thread_safepoint_context(ctx);
  threading::registry::set_current_thread_safepoint_epoch_observed(stop_epoch);
  threading::safepoint::notify_state_change();

  // Block until the stop-the-world epoch is resumed.
  threading::safepoint::wait_while_stop_the_world();
}

pub(crate) fn with_world_stopped_requested(stop_epoch: u64, f: impl FnOnce()) {
  struct ResumeOnDrop;
  impl Drop for ResumeOnDrop {
    fn drop(&mut self) {
      let _ = crate::rt_gc_resume_world();
    }
  }
  let _resume = ResumeOnDrop;

  let timeout = Duration::from_secs(2);
  if !crate::rt_gc_wait_for_world_stopped_timeout(timeout) {
    threading::safepoint::dump_stop_the_world_timeout(stop_epoch, timeout);
    panic!("world did not reach safepoint in time (timeout={timeout:?}, epoch={stop_epoch})");
  }

  // Root enumeration hook. This is intentionally a "plumbing" step: we count
  // roots to validate that:
  // - stop-the-world coordination works across threads
  // - per-thread safepoint context capture is wired
  // - stackmap lookup / stack walking does not crash when available
  let mut roots = 0usize;
  let _ = threading::safepoint::for_each_root_slot_world_stopped(stop_epoch, |_| roots += 1);

  // Also include stack roots for the coordinator thread (if it's a registered mutator). These are
  // not covered by `for_each_root_slot_world_stopped` since the coordinator is not stopped.
  if let Some(thread) = threading::registry::current_thread_state() {
    if let Some(ctx) = thread.safepoint_context() {
      let bounds = thread
        .stack_bounds()
        .and_then(|b| StackBounds::new(b.lo as u64, b.hi as u64).ok());
      let _ =
        visit_reloc_pairs_from_safepoint_context_with_bounds(&ctx, bounds, &mut |_, _| roots += 1);
    }
  }
  let _ = roots;

  f();

  // Ensure all threads have observed the resumed epoch before allowing another
  // stop-the-world request. Without this post-resume barrier, a rapid sequence
  // of `rt_gc_collect` calls can start a new stop-the-world epoch before some
  // threads have returned from the previous safepoint slow path, causing them to
  // miss the brief resumed epoch and remain blocked across requests.
  let resume_epoch = crate::rt_gc_resume_world();
  assert!(
    crate::rt_gc_wait_for_world_resumed_timeout(timeout),
    "world did not resume safepoint in time (timeout={timeout:?}, epoch={resume_epoch})"
  );
}

/// Stop the world, enumerate roots, run `f`, and resume.
///
/// This is a coordinator-side API; mutator threads should normally stop via
/// `rt_gc_safepoint`.
pub fn with_world_stopped(f: impl FnOnce()) {
  let stop_epoch = crate::rt_gc_request_stop_the_world();
  with_world_stopped_requested(stop_epoch, f);
}
