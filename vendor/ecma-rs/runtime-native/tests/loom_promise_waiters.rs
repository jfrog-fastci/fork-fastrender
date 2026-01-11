//! Loom model-checking tests for the `PromiseHeader` waiter registration + wake protocol.
//!
//! Run with:
//!   cargo test -p runtime-native --features loom loom_

#![cfg(feature = "loom")]

use loom::sync::Arc;
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
    let ready = new_ready_queue();
    let promise = Arc::new(PromiseHeader::new());
    let waiter = Box::new(Coroutine::new(1, &ready));
    let waiter_ptr = Box::into_raw(waiter);

    let p1 = promise.clone();
    let waiter_ptr_for_thread = waiter_ptr;
    let t_register = thread::spawn(move || {
      p1.register_waiter(waiter_ptr_for_thread);
    });

    let p2 = promise.clone();
    let t_settle = thread::spawn(move || {
      p2.settle();
    });

    t_register.join().unwrap();
    t_settle.join().unwrap();

    assert_ready_exactly_once(ready_queue_snapshot(&ready), &[1]);

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
    let ready = new_ready_queue();
    let promise = Arc::new(PromiseHeader::new());
    let waiter = Box::new(Coroutine::new(1, &ready));
    let waiter_ptr = Box::into_raw(waiter);

    // Ensure there is something to wake.
    promise.register_waiter(waiter_ptr);

    let p1 = promise.clone();
    let t1 = thread::spawn(move || {
      p1.settle();
    });
    let p2 = promise.clone();
    let t2 = thread::spawn(move || {
      p2.settle();
    });

    t1.join().unwrap();
    t2.join().unwrap();

    assert!(promise.is_settled());
    assert_ready_exactly_once(ready_queue_snapshot(&ready), &[1]);

    unsafe {
      drop(Box::from_raw(waiter_ptr));
    }
  });
}

#[test]
fn loom_two_waiters_no_loss() {
  loom::model::Builder::new().check(|| {
    let ready = new_ready_queue();
    let promise = Arc::new(PromiseHeader::new());

    let w1 = Box::new(Coroutine::new(1, &ready));
    let w2 = Box::new(Coroutine::new(2, &ready));
    let w1_ptr = Box::into_raw(w1);
    let w2_ptr = Box::into_raw(w2);

    let p1 = promise.clone();
    let w1_ptr_for_thread = w1_ptr;
    let t1 = thread::spawn(move || {
      p1.register_waiter(w1_ptr_for_thread);
    });

    let p2 = promise.clone();
    let w2_ptr_for_thread = w2_ptr;
    let t2 = thread::spawn(move || {
      p2.register_waiter(w2_ptr_for_thread);
    });

    // Settle on the main thread to reduce Loom state space (Arc refcounting and
    // thread scheduling adds lots of interleavings).
    promise.settle();

    t1.join().unwrap();
    t2.join().unwrap();

    assert_ready_exactly_once(ready_queue_snapshot(&ready), &[1, 2]);

    unsafe {
      drop(Box::from_raw(w1_ptr));
      drop(Box::from_raw(w2_ptr));
    }
  });
}
