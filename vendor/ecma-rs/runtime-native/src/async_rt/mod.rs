//! Async runtime used by LLVM-generated code.
//!
//! This module provides:
//! - A JS-shaped, single-consumer event loop (microtasks + macrotasks + timers + epoll reactor).
//! - Minimal Promise/coroutine helpers for async/await lowering.
//!
//! The event loop is driven by `rt_async_poll` and is conceptually single-threaded
//! to preserve JS ordering. Other threads may enqueue work; an `eventfd` is used
//! to wake a blocked `epoll_wait`.

mod event_loop;
mod reactor;
mod timer;

pub(crate) mod coroutine;
pub(crate) mod promise;

pub mod gc;

pub use reactor::Interest;
pub use reactor::WatcherId;
pub use timer::TimerId;

use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

pub type TaskFn = extern "C" fn(*mut u8);

#[derive(Clone)]
pub struct Task {
  callback: TaskFn,
  data: *mut u8,
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

impl Task {
  pub fn new(callback: TaskFn, data: *mut u8) -> Self {
    Self {
      callback,
      data,
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
      gc_root: Some(gc::Root::new_unchecked(data)),
    }
  }

  fn run(self) {
    (self.callback)(self.data);
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

  pub fn register_fd(&self, fd: std::os::fd::RawFd, interest: Interest, task: Task) -> std::io::Result<WatcherId> {
    self.loop_.register_fd(fd, interest, task)
  }

  pub fn deregister_fd(&self, id: WatcherId) -> bool {
    self.loop_.deregister_fd(id)
  }
}

static GLOBAL: OnceLock<AsyncRuntime> = OnceLock::new();

pub fn global() -> &'static AsyncRuntime {
  GLOBAL.get_or_init(|| AsyncRuntime::new().expect("failed to initialize runtime-native async runtime"))
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
  global().poll()
}

// -----------------------------------------------------------------------------
// Public convenience helpers.
// -----------------------------------------------------------------------------

pub fn enqueue_microtask(callback: TaskFn, data: *mut u8) {
  global().enqueue_microtask(Task::new(callback, data));
}

pub fn enqueue_macrotask(callback: TaskFn, data: *mut u8) {
  global().enqueue_macrotask(Task::new(callback, data));
}

pub fn schedule_timer(deadline: Instant, callback: TaskFn, data: *mut u8) -> TimerId {
  global().schedule_timer(deadline, Task::new(callback, data))
}

pub fn register_fd(fd: std::os::fd::RawFd, interest: Interest, callback: TaskFn, data: *mut u8) -> std::io::Result<WatcherId> {
  global().register_fd(fd, interest, Task::new(callback, data))
}
