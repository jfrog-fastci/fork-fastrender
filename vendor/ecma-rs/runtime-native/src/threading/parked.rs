use super::registry;
use super::safepoint;
use super::set_parked;

/// RAII guard that marks the current thread as `parked` for the duration of a
/// potentially-blocking runtime operation (locks, syscalls, ...).
///
/// While parked, the stop-the-world coordinator treats the thread as already
/// quiescent and will not wait for it to observe the current safepoint epoch.
pub struct ParkedGuard {
  was_parked: bool,
}

impl ParkedGuard {
  // Do not inline: parked transitions publish a `SafepointContext` via
  // `arch::capture_safepoint_context`, whose assembly helper expects this helper to have its own
  // distinct frame. If this were inlined into the caller, the captured `fp` would skip too far up
  // the stack and refer to the caller's caller.
  #[inline(never)]
  pub fn new() -> Self {
    let thread = registry::current_thread_state();
    let was_parked = thread.as_ref().is_some_and(|s| s.is_parked());

    // The parked state transition must happen at a boundary where the current stack/register set
    // does not contain untracked GC pointers.
    if let Some(thread) = &thread {
      let len = thread.handle_stack_len();
      debug_assert_eq!(
        len,
        0,
        "thread {:?} attempted to park while holding {len} handle-stack roots; \
         store GC references in RootHandles/RootRegistry (stable handles) before blocking",
        thread.id()
      );
    }

    // Only the outermost parked transition needs to publish a safepoint context.
    if !was_parked && thread.is_some() {
      // Publish a safepoint context (for stack walking) before advertising the parked state.
      //
      // Important: call `arch::capture_safepoint_context` directly from this helper frame so it
      // captures the *outer* caller frame that remains live while the thread is blocked.
      let mut ctx = crate::arch::capture_safepoint_context();
      ctx = safepoint::fixup_safepoint_context_to_nearest_managed(ctx, crate::stackmap::try_stackmaps());
      registry::set_current_thread_safepoint_context(ctx);
    }

    if !was_parked {
      registry::set_current_thread_parked(true);
    }
    Self { was_parked }
  }
}

impl Drop for ParkedGuard {
  #[inline]
  fn drop(&mut self) {
    // Preserve an outer/manual parking state (e.g. an already-idle worker).
    if !self.was_parked {
      set_parked(false);
    }
  }
}

/// Execute `f` while treating the current thread as parked.
#[inline]
pub fn park_while<F, R>(f: F) -> R
where
  F: FnOnce() -> R,
{
  let _guard = ParkedGuard::new();
  f()
}
