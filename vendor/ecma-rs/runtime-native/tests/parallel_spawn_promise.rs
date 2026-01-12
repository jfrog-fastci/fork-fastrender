use runtime_native::abi::PromiseRef;
use runtime_native::async_abi::PromiseHeader;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::PromiseLayout;
use std::mem;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

extern "C" fn parallel_add_one_task(data: *mut u8, promise: PromiseRef) {
  unsafe {
    let input = &*(data as *const u32);
    let out = runtime_native::rt_promise_payload_ptr(promise) as *mut u32;
    *out = input.wrapping_add(1);
    runtime_native::rt_promise_fulfill(promise);
  }
}

extern "C" fn parallel_reject_task(data: *mut u8, promise: PromiseRef) {
  // Safety: caller passed `Arc::into_raw(allow.clone()) as *mut u8`.
  let allow = unsafe { Arc::from_raw(data as *const AtomicBool) };
  while !allow.load(Ordering::Acquire) {
    std::thread::yield_now();
  }
  unsafe {
    let out = runtime_native::rt_promise_payload_ptr(promise) as *mut u32;
    *out = 0xDEAD_BEEF_u32;
    runtime_native::rt_promise_reject(promise);
  }
  // `allow` dropped here.
}

#[test]
fn promise_payload_ptr_returns_null_for_non_payload_promises() {
  let _rt = TestRuntimeGuard::new();

  // Allocate a header-only native promise. `PromiseHeader` now includes an internal GC prefix, so
  // construct it via zero-initialization + `rt_promise_init` rather than a struct literal.
  let mut promise_header = Box::new(unsafe { mem::zeroed::<PromiseHeader>() });
  let p = PromiseRef((&mut *promise_header as *mut PromiseHeader).cast());

  // Initialize as a native async-ABI promise (header-only + inline payload owned by codegen).
  unsafe {
    runtime_native::rt_promise_init(p);
  }

  // `rt_promise_payload_ptr` is only defined for `rt_parallel_spawn_promise` payload promises. For
  // any other promise layout (including native `Promise<T>` allocations), it must return null.
  let payload = runtime_native::rt_promise_payload_ptr(p);
  assert!(payload.is_null());
}

#[test]
fn parallel_spawn_promise_fulfill_allows_payload_read_after_block_on() {
  let _rt = TestRuntimeGuard::new();

  let input = Box::new(41u32);
  let input_ptr = Box::into_raw(input);

  let promise = runtime_native::rt_parallel_spawn_promise(
    parallel_add_one_task,
    input_ptr.cast::<u8>(),
    PromiseLayout::of::<u32>(),
  );

  unsafe {
    runtime_native::rt_async_block_on(promise);
  }

  let header = promise.0.cast::<PromiseHeader>();
  assert!(!header.is_null());
  let state = unsafe { &*header }.state.load(Ordering::Acquire);
  assert_eq!(state, PromiseHeader::FULFILLED);

  let payload = runtime_native::rt_promise_payload_ptr(promise) as *const u32;
  assert!(!payload.is_null());
  assert_eq!(unsafe { *payload }, 42);

  unsafe {
    drop(Box::from_raw(input_ptr));
  }
}

#[test]
fn parallel_spawn_promise_reject_allows_payload_read_after_block_on() {
  let _rt = TestRuntimeGuard::new();

  let allow = Arc::new(AtomicBool::new(false));
  let promise = runtime_native::rt_parallel_spawn_promise(
    parallel_reject_task,
    Arc::into_raw(allow.clone()) as *mut u8,
    PromiseLayout::of::<u32>(),
  );

  unsafe {
    // Ensure the promise is marked handled before the worker rejects it. This keeps the test
    // deterministic and avoids relying on a race between rejection and `rt_async_block_on`'s waker
    // registration.
    runtime_native::rt_promise_mark_handled(promise);
    allow.store(true, Ordering::Release);
    runtime_native::rt_async_block_on(promise);
  }

  let header = promise.0.cast::<PromiseHeader>();
  assert!(!header.is_null());
  let state = unsafe { &*header }.state.load(Ordering::Acquire);
  assert_eq!(state, PromiseHeader::REJECTED);

  let payload = runtime_native::rt_promise_payload_ptr(promise) as *const u32;
  assert!(!payload.is_null());
  assert_eq!(unsafe { *payload }, 0xDEAD_BEEF_u32);
}

#[repr(C)]
struct WorkerData {
  input: u32,
  main_thread: std::thread::ThreadId,
  ran_on_other_thread: AtomicBool,
}

extern "C" fn parallel_worker_record_task(data: *mut u8, promise: PromiseRef) {
  unsafe {
    let data = &*(data as *const WorkerData);
    let out = runtime_native::rt_promise_payload_ptr(promise) as *mut u32;
    *out = data.input;
    data
      .ran_on_other_thread
      .store(std::thread::current().id() != data.main_thread, Ordering::Release);
    runtime_native::rt_promise_fulfill(promise);
  }
}

#[test]
fn promise_is_fulfilled_on_worker_thread_and_wakes_event_loop() {
  let _rt = TestRuntimeGuard::new();

  let data = Box::new(WorkerData {
    input: 123,
    main_thread: std::thread::current().id(),
    ran_on_other_thread: AtomicBool::new(false),
  });
  let data_ptr = Box::into_raw(data);

  let promise = runtime_native::rt_parallel_spawn_promise(
    parallel_worker_record_task,
    data_ptr.cast::<u8>(),
    PromiseLayout::of::<u32>(),
  );

  unsafe {
    runtime_native::rt_async_block_on(promise);
  }

  let payload = runtime_native::rt_promise_payload_ptr(promise) as *const u32;
  assert!(!payload.is_null());
  assert_eq!(unsafe { *payload }, 123);

  let ran = unsafe { (*data_ptr).ran_on_other_thread.load(Ordering::Acquire) };
  unsafe {
    drop(Box::from_raw(data_ptr));
  }

  assert!(ran);
}

#[repr(C)]
struct WaitForStart {
  start: AtomicBool,
  work: AtomicUsize,
}

extern "C" fn wait_for_start_then_fulfill(data: *mut u8, promise: PromiseRef) {
  // Safety: caller passed `Arc::into_raw(shared.clone()) as *mut u8`.
  let shared = unsafe { Arc::from_raw(data as *const WaitForStart) };
  while !shared.start.load(Ordering::Acquire) {
    std::thread::yield_now();
  }
  shared.work.fetch_add(1, Ordering::AcqRel);
  unsafe {
    runtime_native::rt_promise_fulfill(promise);
  }
  // `shared` dropped here.
}

#[test]
fn parallel_spawn_promise_wakes_blocked_async_block_on() {
  let _rt = TestRuntimeGuard::new();

  let shared = Arc::new(WaitForStart {
    start: AtomicBool::new(false),
    work: AtomicUsize::new(0),
  });

  let promise = runtime_native::rt_parallel_spawn_promise(
    wait_for_start_then_fulfill,
    Arc::into_raw(shared.clone()) as *mut u8,
    PromiseLayout::of::<()>(),
  );

  let (tx, rx) = mpsc::channel::<()>();
  let handle = std::thread::spawn(move || {
    unsafe {
      runtime_native::rt_async_block_on(promise);
    }
    tx.send(()).unwrap();
  });

  // Wait for the event loop to actually block in `epoll_wait`/`kevent`.
  let start = Instant::now();
  while !runtime_native::async_rt::debug_in_epoll_wait() {
    assert!(
      start.elapsed() < Duration::from_secs(2),
      "timeout waiting for rt_async_block_on to block in the reactor wait syscall"
    );
    std::thread::yield_now();
  }

  shared.start.store(true, Ordering::Release);

  rx.recv_timeout(Duration::from_secs(5))
    .expect("rt_async_block_on did not wake after the parallel task completed");
  handle.join().unwrap();

  assert_eq!(shared.work.load(Ordering::Acquire), 1);
}

#[test]
fn parallel_spawn_promise_legacy_runs_task_on_worker_and_continuation_on_event_loop_thread() {
  let _rt = TestRuntimeGuard::new();

  #[repr(C)]
  struct Data {
    main_thread: std::thread::ThreadId,
    ran_on_other_thread: AtomicBool,
    continuation_on_main_thread: AtomicBool,
    settled: AtomicBool,
  }

  extern "C" fn task(data: *mut u8, promise: PromiseRef) {
    let data = unsafe { &*(data as *const Data) };
    data
      .ran_on_other_thread
      .store(std::thread::current().id() != data.main_thread, Ordering::Release);
    runtime_native::rt_promise_resolve_legacy(promise, core::ptr::null_mut());
  }

  extern "C" fn on_settle(data: *mut u8) {
    let data = unsafe { &*(data as *const Data) };
    data
      .continuation_on_main_thread
      .store(std::thread::current().id() == data.main_thread, Ordering::Release);
    data.settled.store(true, Ordering::Release);
  }

  let data = Box::new(Data {
    main_thread: std::thread::current().id(),
    ran_on_other_thread: AtomicBool::new(false),
    continuation_on_main_thread: AtomicBool::new(false),
    settled: AtomicBool::new(false),
  });
  let data_ptr = Box::into_raw(data);

  let promise = runtime_native::rt_parallel_spawn_promise_legacy(task, data_ptr.cast::<u8>());
  runtime_native::rt_promise_then_legacy(promise, on_settle, data_ptr.cast::<u8>());

  let start = Instant::now();
  while !unsafe { &*data_ptr }.settled.load(Ordering::Acquire) {
    runtime_native::rt_async_poll_legacy();
    assert!(
      start.elapsed() < Duration::from_secs(2),
      "timeout waiting for legacy parallel promise to settle"
    );
  }

  let ran_on_other_thread = unsafe { &*data_ptr }.ran_on_other_thread.load(Ordering::Acquire);
  let continuation_on_main_thread = unsafe { &*data_ptr }
    .continuation_on_main_thread
    .load(Ordering::Acquire);

  unsafe {
    drop(Box::from_raw(data_ptr));
  }

  assert!(ran_on_other_thread);
  assert!(continuation_on_main_thread);
}
