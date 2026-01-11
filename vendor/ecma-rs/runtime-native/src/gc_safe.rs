//! GC-safe ("native") regions.
//!
//! Cooperative stop-the-world (STW) safepoints can deadlock if a mutator thread is
//! blocked in a syscall or contended lock while the GC is waiting for it to
//! reach a safepoint poll.
//!
//! To avoid this, mutator threads may explicitly transition into a **GC-safe
//! region** before they block in native code. While in a GC-safe region, the
//! safepoint coordinator treats the thread as already quiescent: it does not
//! wait for it to reach a cooperative safepoint poll (so its observed safepoint
//! epoch may remain stale) and instead scans roots using the last published
//! safepoint context.
//!
//! # Contract
//! While a thread is in a GC-safe region it must **not** touch or mutate the GC
//! heap (including performing allocations or write barrier operations). Any GC
//! references that must be used by native code need to be pinned/registered
//! elsewhere (future work).

use crate::threading::registry;
use crate::threading::safepoint;
use std::marker::PhantomData;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// RAII guard returned by [`enter_gc_safe_region`].
///
/// Dropping the guard transitions the current thread back to the normal
/// "running" state.
///
/// Guards are nestable: multiple callers may enter GC-safe regions and the
/// thread is considered GC-safe until the outermost guard is dropped.
#[must_use]
pub struct GcSafeGuard {
  thread: Option<Arc<registry::ThreadState>>,
  // Not `Send`/`Sync`: a GC-safe region is a per-thread state transition.
  _not_send: PhantomData<std::rc::Rc<()>>,
}

impl GcSafeGuard {
  /// Exit a GC-safe region without blocking, even if a stop-the-world request is active.
  ///
  /// This is an internal escape hatch used by GC-aware lock acquisition paths to avoid a deadlock
  /// where:
  /// 1) a thread acquires a contended mutex while inside a GC-safe region, then
  /// 2) a stop-the-world request begins between the "is STW active?" check and dropping the guard,
  /// 3) dropping the guard would block while still holding the mutex, preventing the GC coordinator
  ///    from acquiring the mutex to enumerate roots.
  ///
  /// Callers must ensure they do not execute mutator/GC-unsafe code while a stop-the-world request
  /// is active after calling this. Typical usage is:
  /// - clear `NativeSafe` via `exit_no_wait`,
  /// - re-check the global epoch,
  /// - and if STW is active, release any held locks and enter the safepoint slow path.
  pub(crate) fn exit_no_wait(mut self) {
    let Some(thread) = self.thread.take() else {
      // No-op guard (unregistered thread).
      core::mem::forget(self);
      return;
    };

    let depth = thread.native_safe_depth.load(Ordering::Relaxed);
    debug_assert!(depth > 0, "GcSafeGuard underflow");

    if depth > 1 {
      thread.native_safe_depth.store(depth - 1, Ordering::Relaxed);
      core::mem::forget(self);
      return;
    }

    // Outermost guard: clear NativeSafe without waiting for an in-progress stop-the-world.
    thread.native_safe_depth.store(0, Ordering::Release);
    safepoint::notify_state_change();
    core::mem::forget(self);
  }
}

impl Drop for GcSafeGuard {
  fn drop(&mut self) {
    let Some(thread) = &self.thread else {
      return;
    };

    let depth = thread.native_safe_depth.load(Ordering::Relaxed);
    debug_assert!(depth > 0, "GcSafeGuard underflow");

    if depth > 1 {
      thread.native_safe_depth.store(depth - 1, Ordering::Relaxed);
      return;
    }

    // Outermost guard: do not allow resuming mutator execution while a stop-the-world
    // request is active.
    let mut epoch = safepoint::current_epoch();
    if epoch & 1 == 1 && !safepoint::in_stop_the_world() {
      safepoint::wait_while_stop_the_world();
      epoch = safepoint::current_epoch();
    }

    // Publish that we've observed the resumed (even) epoch before clearing NativeSafe.
    //
    // Threads in a GC-safe region are treated as "already quiescent" by the STW coordinator, so they
    // may not run the cooperative safepoint slow path that normally updates the observed epoch on
    // resume. Without this, a thread can exit NativeSafe after the world is resumed but still have
    // an old `safepoint_epoch_observed`, causing the coordinator's post-resume barrier to time out.
    registry::set_current_thread_safepoint_epoch_observed(epoch);

    thread.native_safe_depth.store(0, Ordering::Release);
    safepoint::notify_state_change();
  }
}

/// Enter a GC-safe ("native") region on the current thread.
///
/// If the current thread is not registered with the runtime thread registry, this
/// is a no-op guard.
// Do not inline: entering a GC-safe region publishes a `SafepointContext` via
// `arch::capture_safepoint_context`, whose assembly helper walks the frame-pointer chain and
// expects this function to have its own distinct frame. If this were inlined into the caller, the
// captured `fp` would skip too far up the stack (missing the caller's frame), which would make
// stackmap-based root scanning unsound.
#[inline(never)]
pub fn enter_gc_safe_region() -> GcSafeGuard {
  let Some(thread) = registry::current_thread_state() else {
    return GcSafeGuard {
      thread: None,
      _not_send: PhantomData,
    };
  };

  // Only the outermost transition needs to publish a safepoint context and mark
  // the thread as NativeSafe.
  if thread.native_safe_depth.load(Ordering::Relaxed) == 0 {
    // A NativeSafe thread is treated as *already quiescent* by stop-the-world GC. That is only
    // correct if there are no live GC pointers in registers/stack at this boundary.
    //
    // In Rust runtime code, temporary GC roots are tracked via the per-thread handle stack (see
    // `roots::RootScope`). If any roots are present, the thread is holding GC pointers in local
    // variables, which may also still be live in registers. Entering a GC-safe region in that
    // state would allow a moving GC to proceed without updating those registers, risking
    // use-after-move corruption when the thread resumes.
    //
    // Keep this a debug assertion: production builds should still attempt to make progress.
    let roots = thread.handle_stack_len();
    debug_assert_eq!(
      roots,
      0,
      "thread {:?} entered GC-safe region while holding {roots} handle-stack roots; \
       NativeSafe threads are treated as quiescent by stop-the-world GC, so raw GC pointers must not be live \
       across this boundary; store GC references in RootHandles/RootRegistry (stable handles) before blocking",
      thread.id()
    );

    // Publish a safepoint context *before* advertising NativeSafe to the GC.
    //
    // If we entered the GC-safe region from within runtime-native code, the current callsite may
    // not have an LLVM stackmap record. Recover the nearest managed callsite cursor by walking the
    // frame-pointer chain so stackmap-based root enumeration (for this thread) can still succeed
    // while it is blocked.
    //
    // Important: call `arch::capture_safepoint_context` directly from this helper frame. The
    // capture shim intentionally skips *this* runtime frame to return a context for the outer
    // caller frame that remains live while NativeSafe; calling it through wrappers can introduce an
    // extra stack frame and break that contract.
    let mut ctx = crate::arch::capture_safepoint_context();
    ctx = safepoint::fixup_safepoint_context_to_nearest_managed(ctx, crate::stackmap::try_stackmaps());
    registry::set_current_thread_safepoint_context(ctx);

    thread.native_safe_depth.store(1, Ordering::Release);
    safepoint::notify_state_change();
  } else {
    thread.native_safe_depth.fetch_add(1, Ordering::Relaxed);
  }

  GcSafeGuard {
    thread: Some(thread),
    _not_send: PhantomData,
  }
}
