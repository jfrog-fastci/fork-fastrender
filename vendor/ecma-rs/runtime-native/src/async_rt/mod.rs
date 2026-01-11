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
mod teardown_queue;
mod timer;

pub(crate) mod coroutine;
pub(crate) mod promise;

pub mod gc;
pub mod gc_handle;

pub use reactor::Interest;
pub use reactor::WatcherId;
pub use teardown_queue::{Discard, TeardownQueue};
pub use timer::TimerId;
pub use timer::Timers;

use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Once;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use crate::sync::GcAwareMutex;

/// Global serialization guard for the core poll loop.
static POLL_LOCK: Lazy<GcAwareMutex<()>> = Lazy::new(|| GcAwareMutex::new(()));

/// When enabled, `await` follows JS microtask semantics even when the awaited promise is already
/// settled: the coroutine is resumed in a later microtask turn instead of synchronously.
///
/// Default is `false` to allow a fast-path synchronous resumption (an EXEC.plan-allowed deviation).
static STRICT_AWAIT_YIELDS: AtomicBool = AtomicBool::new(false);

/// Number of pending "external" events that can make progress without an already-queued
/// microtask/macrotask/timer/fd.
///
/// Today this is used by `rt_parallel_spawn_promise`:
/// - after spawning a CPU task, the event loop should block in `rt_async_poll` even when the task
///   queues are empty, and
/// - completion on a worker thread must wake the event loop.
static EXTERNAL_PENDING: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn has_external_pending() -> bool {
  EXTERNAL_PENDING.load(Ordering::Acquire) > 0
}

pub(crate) fn external_pending_inc() {
  EXTERNAL_PENDING.fetch_add(1, Ordering::AcqRel);
  // Ensure a currently-blocking reactor wait syscall (`epoll_wait`/`kevent`) wakes to observe the
  // new pending count.
  global().loop_.wake();
}

pub(crate) fn external_pending_dec() {
  let prev = EXTERNAL_PENDING.fetch_sub(1, Ordering::AcqRel);
  if prev == 0 {
    // Defensive: mismatched decrements are bugs; don't underflow into "huge pending".
    EXTERNAL_PENDING.store(0, Ordering::Release);
  }
  // Wake the event loop so it can re-check `has_external_pending` and avoid sleeping forever when
  // the last external task completes without queueing a microtask.
  global().loop_.wake();
}

pub type TaskFn = extern "C" fn(*mut u8);
pub type TaskDropFn = extern "C" fn(*mut u8);

pub struct Task {
  callback: TaskFn,
  data: *mut u8,
  drop: Option<TaskDropFn>,
  /// Optional GC root(s) that must stay alive until the task is executed.
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
    let data = self.gc_root.as_ref().map(|r| r.ptr()).unwrap_or(self.data);
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

  pub fn wait_for_work(&self) {
    self.loop_.wait_for_work()
  }

  pub fn enqueue_microtask(&self, task: Task) {
    self.loop_.enqueue_microtask(task);
  }

  pub fn enqueue_microtasks(&self, tasks: impl IntoIterator<Item = Task>) {
    self.loop_.enqueue_microtasks(tasks);
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

  pub fn register_io_with_drop(
    &self,
    fd: std::os::fd::RawFd,
    interests: u32,
    cb: extern "C" fn(u32, *mut u8),
    data: *mut u8,
    drop: TaskDropFn,
  ) -> std::io::Result<WatcherId> {
    self.loop_
      .register_io_with_drop(fd, interests, cb, data, drop)
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
  // Wake the global event loop so a thread blocked in the platform reactor wait syscall
  // (`epoll_wait`/`kevent`) can observe a stop-the-world safepoint request.
  global().loop_.wake();
}

pub fn global() -> &'static AsyncRuntime {
  let rt = GLOBAL.get_or_init(|| {
    AsyncRuntime::new().expect("failed to initialize runtime-native async runtime")
  });

  // Register the reactor wake function exactly once so GC stop-the-world
  // requests can wake a thread blocked in the reactor wait syscall.
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
  // Prevent nested calls into the event loop from tasks/microtasks (HTML-style microtask checkpoint
  // semantics). This also avoids deadlocks on `POLL_LOCK` if code attempts to drive the runtime
  // recursively.
  let Some(_checkpoint_guard) = crate::async_runtime::MicrotaskCheckpointGuard::enter() else {
    return false;
  };

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

/// Block the current thread until at least one task is ready.
pub(crate) fn wait_for_work() {
  let _guard = POLL_LOCK.lock();
  global().wait_for_work();
}

/// Test helper: reset the process-global async runtime to a clean idle state.
///
/// This clears:
/// - microtask queue
/// - macrotask queue
/// - timers
/// - I/O watchers
/// - reactor wake state
/// - external pending count
///
/// It does **not** tear down any background worker threads.
pub(crate) fn clear_state_for_tests() {
  // If another thread is currently blocked in the reactor poll inside the event loop, ensure it wakes up
  // so we don't block indefinitely on the poll lock during test cleanup.
  global().loop_.wake();

  let _guard = POLL_LOCK.lock();
  global().loop_.reset_for_tests();
  crate::unhandled_rejection::clear_state_for_tests();

  // Tests should be isolated from configuration toggles.
  STRICT_AWAIT_YIELDS.store(false, Ordering::Release);
  crate::async_runtime::reset_for_tests();
  EXTERNAL_PENDING.store(0, Ordering::Release);
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

/// Test-only signal indicating whether some thread is currently blocked in the reactor wait syscall.
#[doc(hidden)]
pub fn debug_in_epoll_wait() -> bool {
  reactor::debug_in_epoll_wait()
}

/// Test-only helper: number of active timers currently registered with the global event loop.
#[doc(hidden)]
pub fn debug_timer_count() -> usize {
  global().loop_.debug_timers_count()
}

/// Test-only hook: execute `f` while holding the global microtask-queue lock.
///
/// This exists to deterministically reproduce contention scenarios for
/// stop-the-world safepoint coordination.
#[doc(hidden)]
pub fn debug_with_microtasks_lock<R>(f: impl FnOnce() -> R) -> R {
  global().loop_.debug_with_microtasks_lock(f)
}

/// Test-only hook: execute `f` while holding the reactor's watcher-map lock.
#[doc(hidden)]
pub fn debug_with_reactor_watchers_lock<R>(f: impl FnOnce() -> R) -> R {
  global().loop_.debug_with_reactor_watchers_lock(f)
}

#[cfg(test)]
mod tests;
