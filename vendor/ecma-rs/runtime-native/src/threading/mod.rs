//! Thread tracking and GC safepoint coordination.
//!
//! The GC needs:
//! - a list of mutator threads to scan and stop
//! - a global stop-the-world barrier ("safepoint") that mutators can poll
//! - a way to account for threads parked/idle inside the runtime scheduler

pub mod registry;
pub mod safepoint;
mod parked;

pub use registry::all_threads;
pub use registry::thread_counts;
pub use registry::register_current_thread;
pub use registry::unregister_current_thread;
pub use registry::ThreadCounts;
pub use registry::ThreadId;
pub use registry::ThreadKind;
pub use registry::ThreadState;
pub use parked::park_while;
pub use parked::ParkedGuard;

pub use crate::sync::GcAwareMutex;
pub use crate::sync::GcAwareRwLock;

pub use crate::gc_safe::enter_gc_safe_region;
pub use crate::gc_safe::GcSafeGuard;
/// Register a callback that should be invoked whenever the GC requests a
/// stop-the-world safepoint.
///
/// This is used to wake threads blocked in external wait primitives (e.g.
/// the async reactor poll inside `rt_async_poll`).
pub fn register_reactor_waker(waker: fn()) {
  safepoint::register_gc_waker(waker);
}

/// Mark/unmark the current thread as parked (idle) inside the runtime.
///
/// While `parked == true`, the safepoint coordinator treats the thread as
/// *already quiescent* for stop-the-world requests. This avoids requiring the
/// GC to wake idle worker threads that are blocked on unrelated condition
/// variables.
///
/// Invariant (required by future precise GC stack scanning):
/// - The runtime must only mark a thread `parked` at a known safepoint where the
///   stack does not contain untracked GC pointers.
/// - Before executing mutator code after un-parking, the thread must poll a
///   safepoint (e.g. via [`safepoint_poll`]).
pub fn set_parked(parked: bool) {
  let is_registered = registry::current_thread_state().is_some();
  registry::set_current_thread_parked(parked);
  // Leaving the parked/idle state must immediately poll the safepoint barrier
  // so a thread that unblocks during an in-progress stop-the-world GC doesn't
  // resume mutator work without observing the request.
  if !parked && is_registered {
    safepoint_poll();
  }
}

/// Safepoint poll used at compiler-inserted and runtime-inserted safepoints.
#[inline(always)]
pub fn safepoint_poll() {
  safepoint::rt_gc_safepoint();
}
