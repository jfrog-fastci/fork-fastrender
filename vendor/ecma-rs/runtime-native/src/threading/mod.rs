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
/// When transitioning back to `parked == false`, this function performs a
/// safepoint poll before returning. This ensures a thread cannot resume mutator
/// work in the middle of an in-progress stop-the-world request.
///
/// Invariant (required by future precise GC stack scanning):
/// - The runtime must only mark a thread `parked` at a known safepoint where the
///   stack does not contain untracked GC pointers.
/// - Before executing mutator code after un-parking, the thread must poll a
///   safepoint (e.g. via [`safepoint_poll`]).
pub fn set_parked(parked: bool) {
  let thread = registry::current_thread_state();
  let is_registered = thread.is_some();
  if parked {
    // A parked thread is treated as *already quiescent* by stop-the-world GC. That is only correct
    // if there are no live GC pointers in registers/stack at this boundary.
    //
    // In Rust runtime code, temporary GC roots are tracked via the per-thread handle stack (see
    // `roots::RootScope`). If any roots are present, the thread is holding GC pointers in local
    // variables, which may also still be live in registers. Parking in that state would allow a
    // moving GC to proceed without updating those registers, risking use-after-move corruption when
    // the thread resumes from the blocking syscall/lock.
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
  }
  registry::set_current_thread_parked(parked);
  // Leaving the parked/idle state must immediately poll the safepoint barrier
  // so a thread that unblocks during an in-progress stop-the-world GC doesn't
  // resume mutator work without observing the request.
  if !parked && is_registered {
    safepoint_poll();
  }
}

/// Safepoint poll used by runtime-native and embedding code.
///
/// Compiler-generated code should generally inline an `RT_GC_EPOCH` poll and call
/// `rt_gc_safepoint_slow(epoch)` at the callsite instead of calling this helper.
#[inline(always)]
pub fn safepoint_poll() {
  safepoint::rt_gc_safepoint();
}

/// Register the current OS thread with the runtime's thread registry.
///
/// This wrapper also initializes thread-local allocator state used by `rt_alloc`.
pub fn register_current_thread(kind: ThreadKind) -> ThreadId {
  let id = registry::register_current_thread(kind);
  crate::rt_alloc::on_thread_registered(id);
  id
}

/// Unregister the current OS thread from the runtime's thread registry.
///
/// This wrapper also tears down thread-local allocator bookkeeping used by `rt_alloc`.
pub fn unregister_current_thread() {
  if let Some(id) = registry::current_thread_id() {
    crate::rt_alloc::on_thread_unregistered(id);
  }
  registry::unregister_current_thread();
}
