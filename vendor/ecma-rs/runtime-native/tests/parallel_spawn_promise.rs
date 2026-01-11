use runtime_native::abi::{PromiseRef, RtCoroStatus, RtCoroutineHeader, ValueRef};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::PromiseLayout;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

#[test]
fn parallel_spawn_promise_can_be_awaited_from_coroutine() {
  let _rt = TestRuntimeGuard::new();

  extern "C" fn task(data: *mut u8, promise: PromiseRef) {
    unsafe {
      let input = &*(data as *const u32);
      let out = runtime_native::rt_promise_payload_ptr(promise) as *mut u32;
      *out = input.wrapping_add(1);
      runtime_native::rt_promise_fulfill(promise);
    }
  }

  #[repr(C)]
  struct TestCoroutine {
    header: RtCoroutineHeader,
    completed: *mut bool,
    input: *mut u32,
  }

  extern "C" fn resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
    let coro = coro as *mut TestCoroutine;
    assert!(!coro.is_null());

    unsafe {
      match (*coro).header.state {
        0 => {
          let promise = runtime_native::rt_parallel_spawn_promise(
            task,
            (*coro).input as *mut u8,
            PromiseLayout::of::<u32>(),
          );
          runtime_native::rt_coro_await_legacy(&mut (*coro).header, promise, 1);
          RtCoroStatus::Pending
        }
        1 => {
          assert_eq!((*coro).header.await_is_error, 0);
          let payload = (*coro).header.await_value as *const u32;
          assert_eq!(*payload, 42);
          *(*coro).completed = true;
          runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut());
          RtCoroStatus::Done
        }
        other => panic!("unexpected coroutine state: {other}"),
      }
    }
  }

  let mut completed = false;
  let input = Box::new(41u32);
  let input_ptr = Box::into_raw(input);

  let mut coro = Box::new(TestCoroutine {
    header: RtCoroutineHeader {
      resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    completed: &mut completed,
    input: input_ptr,
  });

  runtime_native::rt_async_spawn_legacy(&mut coro.header);
  let start = Instant::now();
  while !completed {
    runtime_native::rt_async_poll_legacy();
    assert!(
      start.elapsed() < Duration::from_secs(2),
      "timeout waiting for parallel promise to settle"
    );
  }

  unsafe {
    drop(Box::from_raw(input_ptr));
  }
  assert!(completed);
}

#[test]
fn promise_is_fulfilled_on_worker_thread_and_wakes_event_loop() {
  let _rt = TestRuntimeGuard::new();

  #[repr(C)]
  struct Data {
    input: u32,
    main_thread: std::thread::ThreadId,
    ran_on_other_thread: AtomicBool,
  }

  extern "C" fn task(data: *mut u8, promise: PromiseRef) {
    unsafe {
      let data = &*(data as *const Data);
      let out = runtime_native::rt_promise_payload_ptr(promise) as *mut u32;
      *out = data.input;
      data
        .ran_on_other_thread
        .store(std::thread::current().id() != data.main_thread, Ordering::Release);
      runtime_native::rt_promise_fulfill(promise);
    }
  }

  #[repr(C)]
  struct TestCoroutine {
    header: RtCoroutineHeader,
    completed: *mut bool,
    data: *mut Data,
  }

  extern "C" fn resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
    let coro = coro as *mut TestCoroutine;
    assert!(!coro.is_null());

    unsafe {
      match (*coro).header.state {
        0 => {
          let promise = runtime_native::rt_parallel_spawn_promise(
            task,
            (*coro).data as *mut u8,
            PromiseLayout::of::<u32>(),
          );
          runtime_native::rt_coro_await_legacy(&mut (*coro).header, promise, 1);
          RtCoroStatus::Pending
        }
        1 => {
          assert_eq!((*coro).header.await_is_error, 0);
          let payload = (*coro).header.await_value as *const u32;
          assert_eq!(*payload, 123);
          *(*coro).completed = true;
          runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut());
          RtCoroStatus::Done
        }
        other => panic!("unexpected coroutine state: {other}"),
      }
    }
  }

  let data = Box::new(Data {
    input: 123,
    main_thread: std::thread::current().id(),
    ran_on_other_thread: AtomicBool::new(false),
  });
  let data_ptr = Box::into_raw(data);

  let mut completed = false;
  let mut coro = Box::new(TestCoroutine {
    header: RtCoroutineHeader {
      resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    completed: &mut completed,
    data: data_ptr,
  });

  runtime_native::rt_async_spawn_legacy(&mut coro.header);
  let start = Instant::now();
  while !completed {
    runtime_native::rt_async_poll_legacy();
    assert!(
      start.elapsed() < Duration::from_secs(2),
      "timeout waiting for parallel promise to settle"
    );
  }

  let ran = unsafe { (*data_ptr).ran_on_other_thread.load(Ordering::Acquire) };
  unsafe {
    drop(Box::from_raw(data_ptr));
  }

  assert!(completed);
  assert!(ran);
}

extern "C" fn inc_atomic(data: *mut u8) {
  // Safety: caller passed `Arc::into_raw(counter.clone()) as *mut u8`.
  let counter = unsafe { Arc::from_raw(data as *const AtomicUsize) };
  counter.fetch_add(1, Ordering::AcqRel);
}

extern "C" fn inc_and_fulfill(data: *mut u8, promise: PromiseRef) {
  // Safety: caller passed `Arc::into_raw(counter.clone()) as *mut u8`.
  let counter = unsafe { Arc::from_raw(data as *const AtomicUsize) };
  counter.fetch_add(1, Ordering::AcqRel);
  unsafe {
    runtime_native::rt_promise_fulfill(promise);
  }
}

extern "C" fn sleep_then_inc_and_fulfill(data: *mut u8, promise: PromiseRef) {
  // Safety: caller passed `Arc::into_raw(counter.clone()) as *mut u8`.
  let counter = unsafe { Arc::from_raw(data as *const AtomicUsize) };
  std::thread::sleep(Duration::from_millis(200));
  counter.fetch_add(1, Ordering::AcqRel);
  unsafe {
    runtime_native::rt_promise_fulfill(promise);
  }
}

#[test]
fn parallel_spawn_promise_wakes_blocked_async_poll() {
  let _rt = TestRuntimeGuard::new();
  let work = Arc::new(AtomicUsize::new(0));
  let continuations = Arc::new(AtomicUsize::new(0));

  let promise = runtime_native::rt_parallel_spawn_promise(
    sleep_then_inc_and_fulfill,
    Arc::into_raw(work.clone()) as *mut u8,
    PromiseLayout::of::<()>(),
  );
  runtime_native::rt_promise_then_legacy(
    promise,
    inc_atomic,
    Arc::into_raw(continuations.clone()) as *mut u8,
  );

  let (tx, rx) = mpsc::channel();
  std::thread::spawn(move || {
    runtime_native::rt_async_poll_legacy();
    tx.send(()).unwrap();
  });

  // The CPU task sleeps; `rt_async_poll_legacy` should block in `epoll_wait` and must not return
  // immediately.
  assert!(
    rx.recv_timeout(Duration::from_millis(50)).is_err(),
    "rt_async_poll_legacy returned before the parallel task completed"
  );

  rx.recv_timeout(Duration::from_secs(5))
    .expect("rt_async_poll_legacy did not wake after the parallel task completed");

  assert_eq!(work.load(Ordering::Acquire), 1);
  assert_eq!(continuations.load(Ordering::Acquire), 1);
}

#[test]
fn parallel_spawn_promise_stress() {
  let _rt = TestRuntimeGuard::new();
  const N: usize = 10_000;

  let work = Arc::new(AtomicUsize::new(0));
  let continuations = Arc::new(AtomicUsize::new(0));

  for _ in 0..N {
    let promise = runtime_native::rt_parallel_spawn_promise(
      inc_and_fulfill,
      Arc::into_raw(work.clone()) as *mut u8,
      PromiseLayout::of::<()>(),
    );
    runtime_native::rt_promise_then_legacy(
      promise,
      inc_atomic,
      Arc::into_raw(continuations.clone()) as *mut u8,
    );
  }

  let start = Instant::now();
  while continuations.load(Ordering::Acquire) < N {
    runtime_native::rt_async_poll_legacy();
    assert!(
      start.elapsed() < Duration::from_secs(10),
      "timeout waiting for {N} parallel promises to settle"
    );
  }

  assert_eq!(work.load(Ordering::Acquire), N);
  assert_eq!(continuations.load(Ordering::Acquire), N);
}

#[test]
fn parallel_spawn_promise_promise_all_like() {
  let _rt = TestRuntimeGuard::new();
  const N: usize = 256;

  extern "C" fn task(data: *mut u8, promise: PromiseRef) {
    unsafe {
      let input = data as usize as u32;
      let out = runtime_native::rt_promise_payload_ptr(promise) as *mut u32;
      *out = input.wrapping_mul(2);
      runtime_native::rt_promise_fulfill(promise);
    }
  }

  #[repr(C)]
  struct AllState {
    remaining: AtomicUsize,
    results: *mut u32,
    all_promise: PromiseRef,
  }

  #[repr(C)]
  struct OneState {
    idx: usize,
    promise: PromiseRef,
    all: *mut AllState,
  }

  extern "C" fn on_one_settle(data: *mut u8) {
    // Safety: allocated as `Box<OneState>` in the test setup and freed by `drop_one_state`.
    let one = unsafe { &*(data as *const OneState) };
    let all = unsafe { &*one.all };

    let payload = runtime_native::rt_promise_payload_ptr(one.promise) as *const u32;
    if !payload.is_null() {
      unsafe {
        *all.results.add(one.idx) = *payload;
      }
    }

    if all.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
      runtime_native::rt_promise_resolve_legacy(all.all_promise, all.results as ValueRef);
    }
  }

  extern "C" fn drop_one_state(data: *mut u8) {
    // Safety: allocated as `Box<OneState>` in the test setup.
    unsafe {
      drop(Box::from_raw(data as *mut OneState));
    }
  }

  #[repr(C)]
  struct AwaitAllCoroutine {
    header: RtCoroutineHeader,
    completed: *mut bool,
    awaited: PromiseRef,
  }

  extern "C" fn await_all_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
    let coro = coro as *mut AwaitAllCoroutine;
    assert!(!coro.is_null());

    unsafe {
      match (*coro).header.state {
        0 => {
          runtime_native::rt_coro_await_legacy(&mut (*coro).header, (*coro).awaited, 1);
          RtCoroStatus::Pending
        }
        1 => {
          *(*coro).completed = true;
          runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut());
          RtCoroStatus::Done
        }
        other => panic!("unexpected coroutine state: {other}"),
      }
    }
  }

  let all_promise = runtime_native::rt_promise_new_legacy();
  let results = vec![0u32; N].into_boxed_slice();
  let results_ptr = Box::into_raw(results) as *mut u32;

  let all_state = Box::new(AllState {
    remaining: AtomicUsize::new(N),
    results: results_ptr,
    all_promise,
  });
  let all_state_ptr = Box::into_raw(all_state);

  for i in 0..N {
    let promise =
      runtime_native::rt_parallel_spawn_promise(task, i as *mut u8, PromiseLayout::of::<u32>());
    let one = Box::new(OneState {
      idx: i,
      promise,
      all: all_state_ptr,
    });
    runtime_native::rt_promise_then_with_drop_legacy(
      promise,
      on_one_settle,
      Box::into_raw(one) as *mut u8,
      drop_one_state,
    );
  }

  let mut completed = false;
  let mut coro = Box::new(AwaitAllCoroutine {
    header: RtCoroutineHeader {
      resume: await_all_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    completed: &mut completed,
    awaited: all_promise,
  });

  runtime_native::rt_async_spawn_legacy(&mut coro.header);
  let start = Instant::now();
  while !completed {
    runtime_native::rt_async_poll_legacy();
    assert!(
      start.elapsed() < Duration::from_secs(10),
      "timeout waiting for {N} parallel promises to settle"
    );
  }

  assert!(completed);

  let results = unsafe { Box::from_raw(results_ptr as *mut [u32; N]) };
  for (i, v) in results.iter().copied().enumerate() {
    assert_eq!(v, (i as u32).wrapping_mul(2));
  }

  unsafe {
    drop(Box::from_raw(all_state_ptr));
  }
}

