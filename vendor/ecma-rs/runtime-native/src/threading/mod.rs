//! Thread tracking and GC safepoint coordination.
//!
//! The GC needs:
//! - a list of mutator threads to scan and stop
//! - a global stop-the-world barrier ("safepoint") that mutators can poll
//! - a way to account for threads parked/idle inside the runtime scheduler

pub mod registry;
pub mod safepoint;

pub use registry::all_threads;
pub use registry::thread_counts;
pub use registry::register_current_thread;
pub use registry::unregister_current_thread;
pub use registry::ThreadCounts;
pub use registry::ThreadId;
pub use registry::ThreadKind;
pub use registry::ThreadState;

pub use crate::gc_safe::enter_gc_safe_region;
pub use crate::gc_safe::GcSafeGuard;
/// Register a callback that should be invoked whenever the GC requests a
/// stop-the-world safepoint.
///
/// This is used to wake threads blocked in external wait primitives (e.g.
/// `epoll_wait` inside `rt_async_poll`).
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
  registry::set_current_thread_parked(parked);
}

/// Safepoint poll used at compiler-inserted and runtime-inserted safepoints.
#[inline(always)]
pub fn safepoint_poll() {
  safepoint::rt_gc_safepoint();
}
