use std::cell::Cell;
use std::ptr;

/// Per-thread mutator state used by the generational write barrier.
#[derive(Debug)]
pub struct MutatorThread {
  /// Newly remembered objects since the last minor GC.
  ///
  /// The write barrier pushes an object here when it transitions its header
  /// `REMEMBERED` bit from 0 → 1. GC start merges all threads' buffers into the
  /// global remembered set.
  pub new_remembered: Vec<*mut u8>,
}

/// Default per-thread capacity for [`MutatorThread::new_remembered`].
///
/// The write barrier is `NoGC` and must not allocate. Callers that install a
/// `MutatorThread` in TLS must ensure the buffer has spare capacity before
/// entering code that may hit the barrier; otherwise the runtime aborts.
pub const DEFAULT_NEW_REMEMBERED_CAPACITY: usize = 4 * 1024;

impl MutatorThread {
  pub fn new() -> Self {
    Self::with_capacity(DEFAULT_NEW_REMEMBERED_CAPACITY)
  }

  pub fn with_capacity(capacity: usize) -> Self {
    Self {
      new_remembered: Vec::with_capacity(capacity),
    }
  }
}

impl Default for MutatorThread {
  fn default() -> Self {
    Self::new()
  }
}

thread_local! {
  static TLS_MUTATOR_THREAD: Cell<*mut MutatorThread> = Cell::new(ptr::null_mut());
}

pub fn current_mutator_thread_ptr() -> *mut MutatorThread {
  // `current_mutator_thread_ptr` can be called from other TLS destructors during thread teardown
  // (e.g. embedding runtimes that clean up write-barrier state from TLS). If this TLS key has
  // already been destroyed, `LocalKey::with` would panic with `AccessError` and abort the process
  // (`abort_on_dtor_unwind`). Treat an inaccessible key as "no mutator thread installed".
  TLS_MUTATOR_THREAD
    .try_with(|c| c.get())
    .unwrap_or(ptr::null_mut())
}

pub fn set_current_mutator_thread_ptr(thread: *mut MutatorThread) {
  let _ = TLS_MUTATOR_THREAD.try_with(|c| c.set(thread));
}

/// RAII guard that installs a thread-local [`MutatorThread`] for the duration
/// of the guard.
///
/// Intended for tests and embedding runtimes.
pub struct ThreadContextGuard {
  prev_thread: *mut MutatorThread,
}

impl ThreadContextGuard {
  pub fn install(thread: &mut MutatorThread) -> Self {
    let prev_thread = current_mutator_thread_ptr();
    set_current_mutator_thread_ptr(thread as *mut MutatorThread);
    Self { prev_thread }
  }
}

impl Drop for ThreadContextGuard {
  fn drop(&mut self) {
    set_current_mutator_thread_ptr(self.prev_thread);
  }
}
