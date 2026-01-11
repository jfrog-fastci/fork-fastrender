//! Loom model-checking tests for the `PromiseHeader` waiter registration + wake protocol.
//!
//! Run with:
//!   # `runtime-native` requires frame pointers; easiest is via the LLVM wrapper:
//!   bash vendor/ecma-rs/scripts/cargo_llvm.sh test -p runtime-native --features loom --test loom_promise_waiters
//!
//!   # Or, if you already have `RUSTFLAGS="-C force-frame-pointers=yes"` configured:
//!   cargo test -p runtime-native --features loom --test loom_promise_waiters
//!   cargo test -p runtime-native --features loom loom_

#![cfg(feature = "loom")]

use loom::thread;
use runtime_native::loom_promise_waiters::ready_queue_snapshot;
use runtime_native::loom_promise_waiters::Coroutine;
use runtime_native::loom_promise_waiters::PromiseHeader;
use runtime_native::loom_promise_waiters::new_ready_queue;

fn assert_ready_exactly_once(got: Vec<usize>, expected: &[usize]) {
  let mut got = got;
  got.sort_unstable();
  let mut expected = expected.to_vec();
  expected.sort_unstable();
  assert_eq!(got, expected);
}

#[test]
fn loom_lost_wakeup_race() {
  loom::model::Builder::new().check(|| {
    let ready = Box::new(new_ready_queue());
    let promise = Box::new(PromiseHeader::new());
    let promise_ptr: *const PromiseHeader = &*promise;

    let waiter = Box::new(Coroutine::new(1, &*ready));
    let waiter_ptr = Box::into_raw(waiter);

    let waiter_ptr_for_thread = waiter_ptr;
    let t_register = thread::spawn(move || {
      unsafe { (&*promise_ptr).register_waiter(waiter_ptr_for_thread) };
    });

    let t_settle = thread::spawn(move || {
      unsafe { (&*promise_ptr).settle() };
    });

    let res_register = t_register.join();
    let res_settle = t_settle.join();
    res_register.unwrap();
    res_settle.unwrap();

    assert_ready_exactly_once(ready_queue_snapshot(&*ready), &[1]);
    assert!(promise.is_settled());
    assert!(promise.waiters_is_empty());

    // SAFETY: all threads have finished and the promise has been settled, so the
    // waiter is no longer reachable from the waiter stack.
    unsafe {
      drop(Box::from_raw(waiter_ptr));
    }
  });
}

#[test]
fn loom_double_settle_no_double_wake() {
  loom::model::Builder::new().check(|| {
    let ready = Box::new(new_ready_queue());
    let promise = Box::new(PromiseHeader::new());
    let promise_ptr: *const PromiseHeader = &*promise;

    let waiter = Box::new(Coroutine::new(1, &*ready));
    let waiter_ptr = Box::into_raw(waiter);

    // Ensure there is something to wake.
    unsafe { promise.register_waiter(waiter_ptr) };

    let t1 = thread::spawn(move || {
      unsafe { (&*promise_ptr).settle() };
    });
    let t2 = thread::spawn(move || {
      unsafe { (&*promise_ptr).settle() };
    });

    let r1 = t1.join();
    let r2 = t2.join();
    r1.unwrap();
    r2.unwrap();

    assert!(promise.is_settled());
    assert_ready_exactly_once(ready_queue_snapshot(&*ready), &[1]);
    assert!(promise.waiters_is_empty());

    unsafe {
      drop(Box::from_raw(waiter_ptr));
    }
  });
}

#[test]
fn loom_two_waiters_no_loss() {
  loom::model::Builder::new().check(|| {
    let ready = Box::new(new_ready_queue());
    let promise = Box::new(PromiseHeader::new());
    let promise_ptr: *const PromiseHeader = &*promise;

    let w1 = Box::new(Coroutine::new(1, &*ready));
    let w2 = Box::new(Coroutine::new(2, &*ready));
    let w1_ptr = Box::into_raw(w1);
    let w2_ptr = Box::into_raw(w2);

    let w1_ptr_for_thread = w1_ptr;
    let t1 = thread::spawn(move || {
      unsafe { (&*promise_ptr).register_waiter(w1_ptr_for_thread) };
    });

    let w2_ptr_for_thread = w2_ptr;
    let t2 = thread::spawn(move || {
      unsafe { (&*promise_ptr).register_waiter(w2_ptr_for_thread) };
    });

    // Settle on the main thread to reduce Loom state space (Arc refcounting and
    // thread scheduling adds lots of interleavings).
    promise.settle();

    let r1 = t1.join();
    let r2 = t2.join();
    r1.unwrap();
    r2.unwrap();

    assert_ready_exactly_once(ready_queue_snapshot(&*ready), &[1, 2]);
    assert!(promise.is_settled());
    assert!(promise.waiters_is_empty());

    unsafe {
      drop(Box::from_raw(w1_ptr));
      drop(Box::from_raw(w2_ptr));
    }
  });
}

#[test]
fn loom_register_after_settle_wakes_immediately() {
  loom::model::Builder::new().check(|| {
    let ready = Box::new(new_ready_queue());
    let promise = PromiseHeader::new();

    // Settle before the waiter registers. Correct protocols must still avoid
    // losing the wakeup (the waiter-side post-push state recheck handles this).
    promise.settle();

    let waiter = Box::new(Coroutine::new(1, &*ready));
    let waiter_ptr = Box::into_raw(waiter);
    unsafe { promise.register_waiter(waiter_ptr) };

    assert_ready_exactly_once(ready_queue_snapshot(&*ready), &[1]);
    assert!(promise.is_settled());
    assert!(promise.waiters_is_empty());

    unsafe {
      drop(Box::from_raw(waiter_ptr));
    }
  });
}

#[test]
fn loom_two_waiters_register_after_settle() {
  loom::model::Builder::new().check(|| {
    let ready = Box::new(new_ready_queue());
    let promise = Box::new(PromiseHeader::new());
    let promise_ptr: *const PromiseHeader = &*promise;

    // Settle before either waiter registers.
    promise.settle();

    let w1 = Box::new(Coroutine::new(1, &*ready));
    let w2 = Box::new(Coroutine::new(2, &*ready));
    let w1_ptr = Box::into_raw(w1);
    let w2_ptr = Box::into_raw(w2);

    let t1 = thread::spawn(move || unsafe {
      (&*promise_ptr).register_waiter(w1_ptr);
    });
    let t2 = thread::spawn(move || unsafe {
      (&*promise_ptr).register_waiter(w2_ptr);
    });

    let r1 = t1.join();
    let r2 = t2.join();
    r1.unwrap();
    r2.unwrap();

    assert_ready_exactly_once(ready_queue_snapshot(&*ready), &[1, 2]);
    assert!(promise.is_settled());
    assert!(promise.waiters_is_empty());

    unsafe {
      drop(Box::from_raw(w1_ptr));
      drop(Box::from_raw(w2_ptr));
    }
  });
}

/// Sanity check: prove the test suite can actually catch the classic lost-wakeup bug.
///
/// If a waiter registers with a lock-free stack but **does not** re-check the promise
/// state after pushing (and call `wake_all()` if already settled), it is possible for:
///   1) the promise to drain the waiter stack while it is still empty, then
///   2) the waiter to push itself after settlement, and
///   3) no one ever drains the stack again.
///
/// Loom should find this interleaving and make `check()` fail; we assert that it does.
#[test]
fn loom_sanity_missing_recheck_is_detected() {
  use loom::sync::atomic::{AtomicPtr, AtomicU8, Ordering};
  use loom::sync::Mutex;
  use std::panic::{catch_unwind, AssertUnwindSafe};

  const PENDING: u8 = 0;
  const SETTLED: u8 = 1;

  struct Waiter {
    id: usize,
    next: AtomicPtr<Waiter>,
    ready: *const Mutex<Vec<usize>>,
  }

  impl Waiter {
    fn new(id: usize, ready: &Mutex<Vec<usize>>) -> Self {
      Self {
        id,
        next: AtomicPtr::new(core::ptr::null_mut()),
        ready: ready as *const Mutex<Vec<usize>>,
      }
    }

    fn wake(&self) {
      unsafe {
        (&*self.ready).lock().unwrap().push(self.id);
      }
    }
  }

  struct BrokenPromise {
    state: AtomicU8,
    waiters: AtomicPtr<Waiter>,
  }

  impl BrokenPromise {
    fn new() -> Self {
      Self {
        state: AtomicU8::new(PENDING),
        waiters: AtomicPtr::new(core::ptr::null_mut()),
      }
    }

    fn register_waiter(&self, waiter_ptr: *mut Waiter) {
      let mut head = self.waiters.load(Ordering::Acquire);
      loop {
        unsafe {
          (*waiter_ptr).next.store(head, Ordering::Relaxed);
        }
        match self
          .waiters
          .compare_exchange(head, waiter_ptr, Ordering::AcqRel, Ordering::Acquire)
        {
          Ok(_) => break,
          Err(new_head) => head = new_head,
        }
      }

      // BUG: missing post-push recheck + wake_all() call when already settled.
    }

    fn settle(&self) {
      self.state.store(SETTLED, Ordering::Release);
      self.wake_all();
    }

    fn wake_all(&self) {
      let mut head = self.waiters.swap(core::ptr::null_mut(), Ordering::AcqRel);
      while !head.is_null() {
        unsafe {
          let waiter = &*head;
          head = waiter.next.load(Ordering::Relaxed);
          waiter.wake();
        }
      }
    }
  }

  let res = catch_unwind(AssertUnwindSafe(|| {
    loom::model::Builder::new().check(|| {
      let ready = Box::new(Mutex::new(Vec::with_capacity(8)));
      let promise = Box::new(BrokenPromise::new());
      let promise_ptr: *const BrokenPromise = &*promise;

      let waiter = Box::new(Waiter::new(1, &*ready));
      let waiter_ptr = Box::into_raw(waiter);

      let t_register = loom::thread::spawn(move || unsafe {
        (&*promise_ptr).register_waiter(waiter_ptr);
      });
      let t_settle = loom::thread::spawn(move || unsafe {
        (&*promise_ptr).settle();
      });

      let r1 = t_register.join();
      let r2 = t_settle.join();
      r1.unwrap();
      r2.unwrap();

      let mut got = ready.lock().unwrap().clone();
      got.sort_unstable();
      assert_eq!(got, vec![1]);

      unsafe {
        drop(Box::from_raw(waiter_ptr));
      }
    });
  }));

  assert!(
    res.is_err(),
    "broken algorithm should be rejected by Loom (expected check() to panic)"
  );
}
