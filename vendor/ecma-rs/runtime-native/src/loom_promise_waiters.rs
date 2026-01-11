//! Loom model-checking harness for the promise waiter registration + wake-up protocol.
//!
//! This code is intentionally small and self-contained:
//! - It models the lock-free protocol using a Treiber stack of waiter nodes.
//! - It is compiled in two modes:
//!   - default: `std::sync` atomics + Mutex
//!   - `--features loom`: `loom::sync` atomics + Mutex
//!
//! The integration tests in `tests/loom_promise_waiters.rs` run under Loom and
//! assert no lost wakeups / no double wakes under all interleavings.

#[cfg(feature = "loom")]
use loom::sync::atomic;
#[cfg(feature = "loom")]
use loom::sync::atomic::Ordering;
#[cfg(feature = "loom")]
use loom::sync::Mutex;

#[cfg(not(feature = "loom"))]
use std::sync::atomic;
#[cfg(not(feature = "loom"))]
use std::sync::atomic::Ordering;
#[cfg(not(feature = "loom"))]
use std::sync::Mutex;

use atomic::AtomicPtr;
use atomic::AtomicU8;

const PROMISE_PENDING: u8 = 0;
const PROMISE_SETTLED: u8 = 1;

/// A minimal "ready queue" used by the Loom tests.
///
/// In the real runtime this would enqueue coroutines/tasks onto the executor.
pub type ReadyQueue = Mutex<Vec<usize>>;

pub fn new_ready_queue() -> ReadyQueue {
  // Pre-allocate a small capacity to avoid `Vec` growth during Loom scheduling,
  // which dramatically increases the explored state space.
  Mutex::new(Vec::with_capacity(8))
}

pub fn ready_queue_snapshot(queue: &ReadyQueue) -> Vec<usize> {
  queue.lock().expect("ready queue mutex poisoned").clone()
}

/// A minimal coroutine header used for modeling the waiter algorithm.
///
/// Each awaiting coroutine registers itself as a waiter in [`PromiseHeader`]
/// using a lock-free Treiber stack (`next_waiter` + CAS into `waiters`).
pub struct Coroutine {
  id: usize,
  next_waiter: AtomicPtr<Coroutine>,
  ready_queue: *const ReadyQueue,
}

impl Coroutine {
  pub fn new(id: usize, ready_queue: &ReadyQueue) -> Self {
    Self {
      id,
      next_waiter: AtomicPtr::new(core::ptr::null_mut()),
      ready_queue: ready_queue as *const ReadyQueue,
    }
  }

  fn wake(&self) {
    // SAFETY: `ready_queue` points to the `ReadyQueue` value; the Loom tests keep
    // that value alive for at least as long as any waiter can be woken.
    unsafe {
      (&*self.ready_queue)
        .lock()
        .expect("ready queue mutex poisoned")
        .push(self.id);
    }
  }
}

/// Promise header containing the lock-free waiter stack and a settled flag.
///
/// The protocol is:
/// - waiter-side registration: push waiter via CAS into `waiters`, then recheck
///   `state`; if already settled, call `wake_all()` (prevents lost wakeups).
/// - settle-side: `state.store(Release)` then `wake_all()`, which swaps the whole
///   waiter stack to null and wakes each waiter exactly once.
pub struct PromiseHeader {
  state: AtomicU8,
  waiters: AtomicPtr<Coroutine>,
}

impl PromiseHeader {
  pub fn new() -> Self {
    Self {
      state: AtomicU8::new(PROMISE_PENDING),
      waiters: AtomicPtr::new(core::ptr::null_mut()),
    }
  }

  pub fn is_settled(&self) -> bool {
    self.state.load(Ordering::Acquire) == PROMISE_SETTLED
  }

  /// Register `waiter` in the waiter stack.
  ///
  /// This is intentionally written in the shape used by the runtime:
  /// - Treiber stack push using CAS.
  /// - Re-check `state` after push; if already settled, call `wake_all()`.
  pub fn register_waiter(&self, waiter_ptr: *mut Coroutine) {
    let mut head = self.waiters.load(Ordering::Acquire);
    loop {
      unsafe {
        (*waiter_ptr).next_waiter.store(head, Ordering::Relaxed);
      }

      match self
        .waiters
        .compare_exchange(head, waiter_ptr, Ordering::AcqRel, Ordering::Acquire)
      {
        Ok(_) => break,
        Err(new_head) => head = new_head,
      }
    }

    // Prevent lost wakeups: if the promise was settled concurrently (possibly
    // before we managed to push), ensure someone drains the waiter stack.
    if self.state.load(Ordering::Acquire) == PROMISE_SETTLED {
      self.wake_all();
    }
  }

  pub fn settle(&self) {
    self.state.store(PROMISE_SETTLED, Ordering::Release);
    self.wake_all();
  }

  fn wake_all(&self) {
    let mut head = self.waiters.swap(core::ptr::null_mut(), Ordering::AcqRel);

    while !head.is_null() {
      unsafe {
        let waiter = &*head;
        head = waiter.next_waiter.load(Ordering::Relaxed);
        waiter.wake();
      }
    }
  }
}
