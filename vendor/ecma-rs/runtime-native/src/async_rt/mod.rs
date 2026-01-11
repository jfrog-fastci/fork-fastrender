//! Async runtime used by LLVM-generated code.
//!
//! This module provides:
//! - A JS-shaped, single-consumer event loop (microtasks + macrotasks + timers + reactor).
//! - Minimal Promise/coroutine helpers for async/await lowering.
//!
//! The event loop is driven by `rt_async_poll` and is conceptually single-threaded
//! to preserve JS ordering. Other threads may enqueue work; a platform-specific waker (e.g.
//! `eventfd` on Linux, `EVFILT_USER` on kqueue platforms) is used to wake a blocked reactor poll.
//!
//! ## Concurrency
//! The runtime is process-global (a singleton). `rt_async_poll` is therefore **thread-safe but not
//! concurrent**: it may be called from multiple threads, but only one thread is allowed to execute
//! the poll loop at a time (calls are internally serialized).

mod event_loop;
mod reactor;
mod timer;

pub(crate) mod coroutine;
pub(crate) mod promise;

pub mod gc;

pub use reactor::Interest;
pub use reactor::WatcherId;
pub use timer::TimerId;
pub use timer::Timers;

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

/// Global serialization guard for the core poll loop.
static POLL_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

/// When enabled, `await` follows JS microtask semantics even when the awaited promise is already
/// settled: the coroutine is resumed in a later microtask turn instead of synchronously.
///
/// Default is `false` to allow a fast-path synchronous resumption (an EXEC.plan-allowed deviation).
static STRICT_AWAIT_YIELDS: AtomicBool = AtomicBool::new(false);

pub type TaskFn = extern "C" fn(*mut u8);
pub type TaskDropFn = extern "C" fn(*mut u8);

pub struct Task {
  callback: TaskFn,
  data: *mut u8,
  drop: Option<TaskDropFn>,
  /// Optional GC root(s) that must stay alive until the task is executed.
  ///
  /// This is a placeholder integration point; the GC itself is not implemented
  /// in this module yet.
  #[allow(dead_code)]
  gc_root: Option<gc::Root>,
}

// The async runtime is multi-producer: other threads may enqueue tasks into the
// shared queues. `Task` carries opaque pointers originating from generated
// code, so we treat it as thread-safe to move between threads.
//
// Safety: the pointer is never dereferenced by the runtime itself; it is
// passed back to the callback on the single-threaded event loop.
unsafe impl Send for Task {}

impl Clone for Task {
  fn clone(&self) -> Self {
    if self.drop.is_some() {
      // `Task` is cloned by the I/O reactor for persistent watchers. Tasks with owned drop hooks are
      // one-shot (e.g. promise reaction jobs) and must not be cloned: cloning would result in
      // double-free when both clones are dropped.
      std::process::abort();
    }
    Self {
      callback: self.callback,
      data: self.data,
      drop: None,
      gc_root: self.gc_root.clone(),
    }
  }
}

impl Drop for Task {
  fn drop(&mut self) {
    let Some(drop) = self.drop else {
      return;
    };
    drop(self.data);
  }
}

impl Task {
  pub fn new(callback: TaskFn, data: *mut u8) -> Self {
    Self {
      callback,
      data,
      drop: None,
      gc_root: None,
    }
  }

  pub fn new_with_drop(callback: TaskFn, data: *mut u8, drop: TaskDropFn) -> Self {
    Self {
      callback,
      data,
      drop: Some(drop),
      gc_root: None,
    }
  }

  /// Create a task whose `data` pointer refers to a GC-managed object that must
  /// be kept alive until the task runs.
  ///
  /// # Safety
  /// `data` must be a valid pointer to a GC-managed object for the eventual GC
  /// implementation.
  pub unsafe fn new_gc_rooted(callback: TaskFn, data: *mut u8) -> Self {
    Self {
      callback,
      data,
      drop: None,
      gc_root: Some(gc::Root::new_unchecked(data)),
    }
  }

  fn run(self) {
    let data = self
      .gc_root
      .as_ref()
      .map(|r| r.ptr())
      .unwrap_or(self.data);
    (self.callback)(data);
  }
}

pub struct AsyncRuntime {
  loop_: event_loop::EventLoop,
}

impl AsyncRuntime {
  pub fn new() -> std::io::Result<Self> {
    Ok(Self {
      loop_: event_loop::EventLoop::new()?,
    })
  }

  pub fn poll(&self) -> bool {
    self.loop_.poll()
  }

  pub fn enqueue_microtask(&self, task: Task) {
    self.loop_.enqueue_microtask(task);
  }

  pub fn enqueue_macrotask(&self, task: Task) {
    self.loop_.enqueue_macrotask(task);
  }

  pub fn schedule_timer(&self, deadline: Instant, task: Task) -> TimerId {
    self.loop_.schedule_timer(deadline, task)
  }

  pub fn schedule_timer_in(&self, delay: Duration, task: Task) -> TimerId {
    self.schedule_timer(Instant::now() + delay, task)
  }

  pub fn cancel_timer(&self, id: TimerId) -> bool {
    self.loop_.cancel_timer(id)
  }

  pub fn register_fd(
    &self,
    fd: std::os::fd::RawFd,
    interest: Interest,
    task: Task,
  ) -> std::io::Result<WatcherId> {
    self.loop_.register_fd(fd, interest, task)
  }

  pub fn register_io(
    &self,
    fd: std::os::fd::RawFd,
    interests: u32,
    cb: extern "C" fn(u32, *mut u8),
    data: *mut u8,
  ) -> std::io::Result<WatcherId> {
    self.loop_.register_io(fd, interests, cb, data)
  }

  pub fn update_io(&self, id: WatcherId, interests: u32) -> bool {
    self.loop_.update_io(id, interests)
  }

  pub fn deregister_fd(&self, id: WatcherId) -> bool {
    self.loop_.deregister_fd(id)
  }
}

static GLOBAL: OnceLock<AsyncRuntime> = OnceLock::new();
static SAFEPOINT_REACTOR_WAKER_ONCE: Once = Once::new();

fn wake_event_loop_for_safepoint() {
  // Wake the global event loop so a thread blocked in `epoll_wait` can observe
  // a stop-the-world safepoint request.
  global().loop_.wake();
}

pub fn global() -> &'static AsyncRuntime {
  let rt = GLOBAL.get_or_init(|| AsyncRuntime::new().expect("failed to initialize runtime-native async runtime"));

  // Register the reactor wake function exactly once so GC stop-the-world
  // requests can wake a thread blocked in `epoll_wait`.
  SAFEPOINT_REACTOR_WAKER_ONCE.call_once(|| {
    crate::threading::register_reactor_waker(wake_event_loop_for_safepoint);
  });

  rt
}

// -----------------------------------------------------------------------------
// Queueing helpers used by the promise/coroutine lowering.
// -----------------------------------------------------------------------------

pub(crate) fn queue_microtask(func: TaskFn, data: *mut u8) {
  global().enqueue_microtask(Task::new(func, data));
}

pub(crate) fn queue_macrotask(func: TaskFn, data: *mut u8) {
  global().enqueue_macrotask(Task::new(func, data));
}

/// Drive the async runtime for one event-loop turn.
///
/// Returns `true` if there is still pending work after the turn.
pub(crate) fn poll() -> bool {
  // Serialize at the ABI boundary: the event loop itself is single-consumer.
  let _guard = POLL_LOCK.lock();
  global().poll()
}

pub(crate) fn drain_microtasks_nonblocking() -> bool {
  let _guard = POLL_LOCK.lock();
  global().loop_.drain_microtasks_for_external()
}

pub(crate) fn run_until_idle_nonblocking() -> bool {
  let _guard = POLL_LOCK.lock();
  global().loop_.run_until_idle_nonblocking()
}

/// Test helper: reset the process-global async runtime to a clean idle state.
///
/// This clears:
/// - microtask queue
/// - macrotask queue
/// - timers
/// - I/O watchers
/// - wake eventfd counter
///
/// It does **not** tear down any background worker threads (the current runtime
/// doesn't spawn any; this is future-proofing for when it does).
pub(crate) fn clear_state_for_tests() {
  // If another thread is currently blocked in the reactor poll inside the event loop, ensure it wakes up
  // so we don't block indefinitely on the poll lock during test cleanup.
  global().loop_.wake();

  let _guard = POLL_LOCK.lock();
  global().loop_.reset_for_tests();

  // Tests should be isolated from configuration toggles.
  STRICT_AWAIT_YIELDS.store(false, Ordering::Release);
}

// -----------------------------------------------------------------------------
// Public convenience helpers.
// -----------------------------------------------------------------------------

/// Configure whether `await` on an already-settled promise yields (strict JS semantics) or resumes
/// synchronously (fast-path).
///
/// - `true` (strict): `await Promise.resolve(x)` resumes in a later microtask turn.
/// - `false` (default): `await Promise.resolve(x)` resumes synchronously during coroutine driving.
pub fn set_strict_await_yields(strict: bool) {
  STRICT_AWAIT_YIELDS.store(strict, Ordering::Release);
}

pub(crate) fn strict_await_yields() -> bool {
  STRICT_AWAIT_YIELDS.load(Ordering::Acquire)
}

pub fn enqueue_microtask(callback: TaskFn, data: *mut u8) {
  global().enqueue_microtask(Task::new(callback, data));
}

pub fn enqueue_macrotask(callback: TaskFn, data: *mut u8) {
  global().enqueue_macrotask(Task::new(callback, data));
}

pub fn schedule_timer(deadline: Instant, callback: TaskFn, data: *mut u8) -> TimerId {
  global().schedule_timer(deadline, Task::new(callback, data))
}

pub fn register_fd(
  fd: std::os::fd::RawFd,
  interest: Interest,
  callback: TaskFn,
  data: *mut u8,
) -> std::io::Result<WatcherId> {
  global().register_fd(fd, interest, Task::new(callback, data))
}

/// Test-only signal indicating whether some thread is currently blocked in `epoll_wait`.
#[doc(hidden)]
pub fn debug_in_epoll_wait() -> bool {
  reactor::debug_in_epoll_wait()
}

/// Test-only helper: number of active timers currently registered with the global event loop.
#[doc(hidden)]
pub fn debug_timer_count() -> usize {
  global().loop_.debug_timers_count()
}
