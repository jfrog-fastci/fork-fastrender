use runtime_native::promise_api::{
  rt_take_rejection_handled, rt_take_unhandled_rejections, AggregateError, Promise, PromiseExt,
  PromiseRejection, Settled,
};
use runtime_native::rt_drain_microtasks;
use runtime_native::test_util::TestRuntimeGuard;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::thread;

fn block_on<F: Future>(fut: F) -> F::Output {
  struct ParkState {
    thread: thread::Thread,
    woken: std::sync::atomic::AtomicBool,
  }

  unsafe fn clone(data: *const ()) -> RawWaker {
    let state = Arc::<ParkState>::from_raw(data as *const ParkState);
    let cloned = state.clone();
    std::mem::forget(state);
    RawWaker::new(Arc::into_raw(cloned) as *const (), &VTABLE)
  }

  unsafe fn wake(data: *const ()) {
    let state = Arc::<ParkState>::from_raw(data as *const ParkState);
    state.woken.store(true, std::sync::atomic::Ordering::Release);
    state.thread.unpark();
    // Drop the Arc.
  }

  unsafe fn wake_by_ref(data: *const ()) {
    let state = Arc::<ParkState>::from_raw(data as *const ParkState);
    state.woken.store(true, std::sync::atomic::Ordering::Release);
    state.thread.unpark();
    std::mem::forget(state);
  }

  unsafe fn drop_waker(data: *const ()) {
    drop(Arc::<ParkState>::from_raw(data as *const ParkState));
  }

  static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);

  let state = Arc::new(ParkState {
    thread: thread::current(),
    woken: std::sync::atomic::AtomicBool::new(false),
  });
  let waker = unsafe { Waker::from_raw(RawWaker::new(Arc::into_raw(state.clone()) as *const (), &VTABLE)) };
  let mut cx = Context::from_waker(&waker);

  let mut fut = Box::pin(fut);
  loop {
    if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
      return out;
    }
    while !state
      .woken
      .swap(false, std::sync::atomic::Ordering::AcqRel)
    {
      thread::park();
    }
  }
}

#[test]
fn then_is_microtask() {
  let _rt = TestRuntimeGuard::new();

  let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

  let (p, resolve, _reject) = Promise::<i32>::new();
  let log2 = log.clone();
  let _p2: Arc<Promise<i32>> = p.then_ok(
    move |v| {
      log2.lock().unwrap().push("then");
      v
    },
  );

  resolve.resolve(1);
  assert!(log.lock().unwrap().is_empty(), "then ran inline");

  rt_drain_microtasks();
  assert_eq!(&*log.lock().unwrap(), &["then"]);
}

#[test]
fn then_catch_finally_ordering() {
  let _rt = TestRuntimeGuard::new();

  let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

  // Reject path: catch runs before downstream then.
  let (p, _resolve, reject) = Promise::<i32>::new();

  let log_catch = log.clone();
  let p2: Arc<Promise<i32>> = p.catch(move |e| {
    assert!(e.downcast_ref::<&'static str>().is_some());
    log_catch.lock().unwrap().push("catch");
    7
  });

  let log_then = log.clone();
  let _p3: Arc<Promise<i32>> = p2.then_ok(
    move |v| {
      assert_eq!(v, 7);
      log_then.lock().unwrap().push("then");
      v
    },
  );

  let log_finally = log.clone();
  let _p4: Arc<Promise<i32>> = p2.finally(move || {
    log_finally.lock().unwrap().push("finally");
    ()
  });

  reject.reject(PromiseRejection::new("nope"));
  rt_drain_microtasks();

  // `finally` is attached to `p2`, so it runs after `catch` settled `p2`.
  assert_eq!(&*log.lock().unwrap(), &["catch", "then", "finally"]);
}

#[test]
fn promise_flattening_adopts_returned_promise() {
  let _rt = TestRuntimeGuard::new();

  let (p, resolve, _reject) = Promise::<i32>::new();

  let inner_resolver: Arc<Mutex<Option<runtime_native::promise_api::PromiseResolver<i32>>>> =
    Arc::new(Mutex::new(None));

  let inner_resolver2 = inner_resolver.clone();
  let p2: Arc<Promise<i32>> = p.then_ok(
    move |_v| {
      let (inner, inner_resolve, _inner_reject) = Promise::<i32>::new();
      *inner_resolver2.lock().unwrap() = Some(inner_resolve);
      inner
    },
  );

  resolve.resolve(1);
  rt_drain_microtasks();

  let got: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
  let got2 = got.clone();
  let _ = p2.then_ok(
    move |v| {
      *got2.lock().unwrap() = Some(v);
      v
    },
  );
  rt_drain_microtasks();
  assert_eq!(*got.lock().unwrap(), None);

  let inner_resolve = inner_resolver
    .lock()
    .unwrap()
    .clone()
    .expect("inner resolver not set");
  inner_resolve.resolve(42);
  rt_drain_microtasks();
  assert_eq!(*got.lock().unwrap(), Some(42));

  let out = block_on(p2.into_future()).unwrap();
  assert_eq!(out, 42);
}

#[test]
fn unhandled_rejection_tracking() {
  let _rt = TestRuntimeGuard::new();

  // Unhandled.
  let (p, _resolve, reject) = Promise::<()>::new();
  let pid = Arc::as_ptr(&p) as usize;
  reject.reject(PromiseRejection::new("boom"));
  rt_drain_microtasks();
  let unhandled = rt_take_unhandled_rejections();
  assert_eq!(unhandled.len(), 1);
  assert_eq!(unhandled[0].promise_id, pid);
  assert_eq!(unhandled[0].reason.downcast_ref::<&'static str>(), Some(&"boom"));

  // Handled-after-the-fact => rejectionhandled notification.
  let _ = p.catch(|_| ());
  let handled = rt_take_rejection_handled();
  assert_eq!(handled.len(), 1);
  assert_eq!(handled[0].promise_id, pid);

  // Handled-before-checkpoint => no unhandled.
  let (p2, _resolve2, reject2) = Promise::<()>::new();
  let _ = p2.catch(|_| ());
  reject2.reject(PromiseRejection::new("nope"));
  rt_drain_microtasks();
  assert!(rt_take_unhandled_rejections().is_empty());
}

#[test]
fn promise_all_mixed_results() {
  let _rt = TestRuntimeGuard::new();

  // Ordering: resolve out-of-order but preserve input order.
  let (p1, r1, _j1) = Promise::<i32>::new();
  let (p2, r2, _j2) = Promise::<i32>::new();
  let all: Arc<Promise<Vec<i32>>> = Promise::all([p1.clone(), p2.clone()]);

  let got: Arc<Mutex<Option<Vec<i32>>>> = Arc::new(Mutex::new(None));
  let got2 = got.clone();
  let _ = all.then(
    move |values| {
      *got2.lock().unwrap() = Some(values);
      ()
    },
    Some(|e| {
      panic!("Promise.all unexpectedly rejected: {:?}", e);
      #[allow(unreachable_code)]
      ()
    }),
  );

  r2.resolve(2);
  r1.resolve(1);
  rt_drain_microtasks();
  assert_eq!(&*got.lock().unwrap(), &Some(vec![1, 2]));

  // Rejection: rejects on first rejection.
  let (p3, r3, _j3) = Promise::<i32>::new();
  let (p4, _r4, j4) = Promise::<i32>::new();
  let all2: Arc<Promise<Vec<i32>>> = Promise::all([p3.clone(), p4.clone()]);

  let got_err: Arc<Mutex<Option<&'static str>>> = Arc::new(Mutex::new(None));
  let got_err2 = got_err.clone();
  let _ = all2.then(
    move |_values| {
      panic!("Promise.all unexpectedly fulfilled");
      #[allow(unreachable_code)]
      ()
    },
    Some(move |e: PromiseRejection| {
      *got_err2.lock().unwrap() = e.downcast_ref::<&'static str>().copied();
      ()
    }),
  );

  r3.resolve(1);
  j4.reject(PromiseRejection::new("no"));
  rt_drain_microtasks();
  assert_eq!(&*got_err.lock().unwrap(), &Some("no"));
}

#[test]
fn concurrency_cross_thread_resolution_wakes_future_and_schedules_reactions() {
  let _rt = TestRuntimeGuard::new();

  let (p, resolve, _reject) = Promise::<i32>::new();

  let log: Arc<Mutex<Vec<i32>>> = Arc::new(Mutex::new(Vec::new()));
  let log2 = log.clone();
  let _ = p.then_ok(
    move |v| {
      log2.lock().unwrap().push(v);
      v
    },
  );

  let resolve2 = resolve.clone();
  let t = thread::spawn(move || {
    resolve2.resolve(99);
  });

  let res = block_on(p.into_future()).unwrap();
  assert_eq!(res, 99);
  t.join().unwrap();

  // Reaction should be queued as a microtask (not run inline on the resolver thread).
  assert!(log.lock().unwrap().is_empty());
  rt_drain_microtasks();
  assert_eq!(&*log.lock().unwrap(), &[99]);
}

#[test]
fn promise_race_resolves_or_rejects_first() {
  let _rt = TestRuntimeGuard::new();

  let (p1, r1, _j1) = Promise::<i32>::new();
  let (p2, _r2, j2) = Promise::<i32>::new();
  let race: Arc<Promise<i32>> = Promise::<i32>::race([p1.clone(), p2.clone()]);

  let got: Arc<Mutex<Option<Result<i32, &'static str>>>> = Arc::new(Mutex::new(None));
  let got2 = got.clone();
  let got3 = got.clone();
  let _ = race.then(
    move |v| {
      *got2.lock().unwrap() = Some(Ok(v));
      ()
    },
    Some(move |e: PromiseRejection| {
      *got3.lock().unwrap() = Some(Err(e.downcast_ref::<&'static str>().copied().unwrap()));
      ()
    }),
  );

  // First settlement wins.
  j2.reject(PromiseRejection::new("no"));
  r1.resolve(1);
  rt_drain_microtasks();
  assert_eq!(*got.lock().unwrap(), Some(Err("no")));
}

#[test]
fn promise_all_settled_preserves_order() {
  let _rt = TestRuntimeGuard::new();

  let (p1, r1, _j1) = Promise::<i32>::new();
  let (p2, _r2, j2) = Promise::<i32>::new();
  let all: Arc<Promise<Vec<Settled<i32>>>> = Promise::<i32>::all_settled([p1.clone(), p2.clone()]);

  let got: Arc<Mutex<Option<Vec<Settled<i32>>>>> = Arc::new(Mutex::new(None));
  let got2 = got.clone();
  let _ = all.then_ok(move |v| {
    *got2.lock().unwrap() = Some(v);
    ()
  });

  // Settle out-of-order.
  j2.reject(PromiseRejection::new("e"));
  r1.resolve(1);
  rt_drain_microtasks();

  let out = got.lock().unwrap().clone().expect("allSettled missing");
  assert!(matches!(out[0], Settled::Fulfilled(1)));
  match &out[1] {
    Settled::Rejected(r) => assert_eq!(r.downcast_ref::<&'static str>(), Some(&"e")),
    _ => panic!("expected rejection"),
  }
}

#[test]
fn promise_any_aggregate_error() {
  let _rt = TestRuntimeGuard::new();

  // First fulfilled wins.
  let (p1, _r1, j1) = Promise::<i32>::new();
  let (p2, r2, _j2) = Promise::<i32>::new();
  let any: Arc<Promise<i32>> = Promise::<i32>::any([p1.clone(), p2.clone()]);

  let got: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
  let got2 = got.clone();
  let _ = any.then_ok(move |v| {
    *got2.lock().unwrap() = Some(v);
    ()
  });

  j1.reject(PromiseRejection::new("a"));
  r2.resolve(2);
  rt_drain_microtasks();
  assert_eq!(*got.lock().unwrap(), Some(2));

  // All rejected => AggregateError with ordered reasons.
  let (p3, _r3, j3) = Promise::<i32>::new();
  let (p4, _r4, j4) = Promise::<i32>::new();
  let any2: Arc<Promise<i32>> = Promise::<i32>::any([p3.clone(), p4.clone()]);

  let got_err: Arc<Mutex<Option<Vec<&'static str>>>> = Arc::new(Mutex::new(None));
  let got_err2 = got_err.clone();
  let _ = any2.then(
    move |_v| {
      panic!("Promise.any unexpectedly fulfilled");
      #[allow(unreachable_code)]
      ()
    },
    Some(move |e: PromiseRejection| {
      let agg = e
        .downcast_ref::<AggregateError>()
        .expect("Promise.any should reject with AggregateError");
      let reasons = agg
        .errors
        .iter()
        .map(|r| *r.downcast_ref::<&'static str>().unwrap())
        .collect::<Vec<_>>();
      *got_err2.lock().unwrap() = Some(reasons);
      ()
    }),
  );

  j3.reject(PromiseRejection::new("x"));
  j4.reject(PromiseRejection::new("y"));
  rt_drain_microtasks();
  assert_eq!(*got_err.lock().unwrap(), Some(vec!["x", "y"]));
}
