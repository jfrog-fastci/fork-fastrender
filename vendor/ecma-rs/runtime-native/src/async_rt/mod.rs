//! Minimal async runtime used by LLVM-generated code.

pub(crate) mod coroutine;
pub(crate) mod promise;

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

type TaskFn = extern "C" fn(*mut u8);

#[derive(Clone, Copy)]
struct Task {
  func: TaskFn,
  data: *mut u8,
}

// The async runtime is single-threaded in terms of execution (microtasks are run by whichever
// thread drives `rt_async_poll`), but tasks can be enqueued from other threads in the future.
// Treating the opaque `data` pointer as `Send` is sound as long as those tasks are only *executed*
// on the owning event loop thread.
unsafe impl Send for Task {}

#[derive(Default)]
struct AsyncRt {
  microtasks: VecDeque<Task>,
  macrotasks: VecDeque<Task>,
}

static ASYNC_RT: OnceLock<Mutex<AsyncRt>> = OnceLock::new();

fn with_rt<R>(f: impl FnOnce(&mut AsyncRt) -> R) -> R {
  let rt = ASYNC_RT.get_or_init(|| Mutex::new(AsyncRt::default()));
  let mut guard = rt.lock().expect("async runtime mutex poisoned");
  f(&mut guard)
}

pub(crate) fn queue_microtask(func: TaskFn, data: *mut u8) {
  with_rt(|rt| rt.microtasks.push_back(Task { func, data }));
}

pub(crate) fn queue_macrotask(func: TaskFn, data: *mut u8) {
  with_rt(|rt| rt.macrotasks.push_back(Task { func, data }));
}

fn pop_microtask() -> Option<Task> {
  with_rt(|rt| rt.microtasks.pop_front())
}

fn pop_macrotask() -> Option<Task> {
  with_rt(|rt| rt.macrotasks.pop_front())
}

/// Drive the async runtime.
///
/// Returns `true` if any work was performed.
///
/// Semantics:
/// - Run **all** pending microtasks (FIFO).
/// - If there are no microtasks, run a single macrotask, then drain microtasks again.
pub(crate) fn poll() -> bool {
  let mut did_work = false;

  // Microtask checkpoint.
  while let Some(task) = pop_microtask() {
    did_work = true;
    (task.func)(task.data);
  }

  // Run one macrotask if available, then do another microtask checkpoint.
  if let Some(task) = pop_macrotask() {
    did_work = true;
    (task.func)(task.data);

    while let Some(task) = pop_microtask() {
      (task.func)(task.data);
    }
  }

  did_work
}
