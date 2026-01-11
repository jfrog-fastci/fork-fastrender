//! GC-safe ("native") regions.
//!
//! Cooperative stop-the-world (STW) safepoints can deadlock if a mutator thread is
//! blocked in a syscall or contended lock while the GC is waiting for it to
//! reach a safepoint poll.
//!
//! To avoid this, mutator threads may explicitly transition into a **GC-safe
//! region** before they block in native code. While in a GC-safe region, the
//! safepoint coordinator treats the thread as already stopped and will scan it
//! using the last published safepoint context.
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

impl Drop for GcSafeGuard {
  fn drop(&mut self) {
    let Some(thread) = &self.thread else {
      return;
    };

    let depth = thread.native_safe_depth.load(Ordering::Relaxed);
    debug_assert!(depth > 0, "GcSafeGuard underflow");

    if depth > 1 {
      thread
        .native_safe_depth
        .store(depth - 1, Ordering::Relaxed);
      return;
    }

    // Outermost guard: do not allow resuming mutator execution while a stop-the-world
    // request is active.
    if safepoint::current_epoch() & 1 == 1 {
      safepoint::wait_while_stop_the_world();
    }

    thread.native_safe_depth.store(0, Ordering::Release);
    safepoint::notify_state_change();
  }
}

/// Enter a GC-safe ("native") region on the current thread.
///
/// If the current thread is not registered with the runtime thread registry, this
/// is a no-op guard.
#[inline]
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
    // Publish a safepoint context *before* advertising NativeSafe to the GC.
    let ctx = crate::arch::capture_safepoint_context();
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
