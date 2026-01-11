use runtime_native::abi::{PromiseRef, RtCoroStatus, RtCoroutineHeader, RtShapeDescriptor, RtShapeId, ValueRef};
use runtime_native::async_abi::PromiseHeader;
use runtime_native::gc::ObjHeader;
use runtime_native::shape_table;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::PromiseLayout;
use std::mem;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Once};
use std::time::{Duration, Instant};

#[repr(C)]
struct GcBox<T> {
  header: ObjHeader,
  payload: T,
}

static SHAPE_TABLE_ONCE: Once = Once::new();
static EMPTY_PTR_OFFSETS: [u32; 0] = [];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    static SHAPES: [RtShapeDescriptor; 4] = [
      RtShapeDescriptor {
        size: mem::size_of::<GcBox<AwaitParallelPromiseCoroutine>>() as u32,
        align: 16,
        flags: 0,
        ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: 0,
        reserved: 0,
      },
      RtShapeDescriptor {
        size: mem::size_of::<GcBox<AwaitWorkerWakeCoroutine>>() as u32,
        align: 16,
        flags: 0,
        ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: 0,
        reserved: 0,
      },
      RtShapeDescriptor {
        size: mem::size_of::<GcBox<AwaitAllCoroutine>>() as u32,
        align: 16,
        flags: 0,
        ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: 0,
        reserved: 0,
      },
      RtShapeDescriptor {
        size: mem::size_of::<GcBox<AwaitParallelRejectPromiseCoroutine>>() as u32,
        align: 16,
        flags: 0,
        ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: 0,
        reserved: 0,
      },
    ];
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

unsafe fn alloc_pinned<T>(shape: RtShapeId) -> *mut GcBox<T> {
  ensure_shape_table();
  runtime_native::rt_alloc_pinned(mem::size_of::<GcBox<T>>(), shape).cast::<GcBox<T>>()
}

extern "C" fn parallel_add_one_task(data: *mut u8, promise: PromiseRef) {
  unsafe {
    let input = &*(data as *const u32);
    let out = runtime_native::rt_promise_payload_ptr(promise) as *mut u32;
    *out = input.wrapping_add(1);
    runtime_native::rt_promise_fulfill(promise);
  }
}

extern "C" fn parallel_reject_task(_data: *mut u8, promise: PromiseRef) {
  unsafe {
    let out = runtime_native::rt_promise_payload_ptr(promise) as *mut u32;
    *out = 0xDEAD_BEEF_u32;
    runtime_native::rt_promise_reject(promise);
  }
}

#[repr(C)]
struct AwaitParallelPromiseCoroutine {
  header: RtCoroutineHeader,
  completed: *mut bool,
  input: *mut u32,
}

extern "C" fn await_parallel_promise_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut AwaitParallelPromiseCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        let promise = runtime_native::rt_parallel_spawn_promise(
          parallel_add_one_task,
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

#[repr(C)]
struct AwaitParallelRejectPromiseCoroutine {
  header: RtCoroutineHeader,
  completed: *mut bool,
}

extern "C" fn await_parallel_reject_promise_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut AwaitParallelRejectPromiseCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        let promise = runtime_native::rt_parallel_spawn_promise(
          parallel_reject_task,
          core::ptr::null_mut(),
          PromiseLayout::of::<u32>(),
        );
        runtime_native::rt_coro_await_legacy(&mut (*coro).header, promise, 1);
        RtCoroStatus::Pending
      }
      1 => {
        assert_eq!((*coro).header.await_is_error, 1);
        assert!(!(*coro).header.await_error.is_null());
        assert_eq!(
          *((*coro).header.await_error as *const u32),
          0xDEAD_BEEF_u32
        );
        assert!((*coro).header.await_value.is_null());
        *(*coro).completed = true;
        runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut());
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
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

#[repr(C)]
struct AwaitWorkerWakeCoroutine {
  header: RtCoroutineHeader,
  completed: *mut bool,
  data: *mut WorkerData,
}

extern "C" fn await_worker_wake_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut AwaitWorkerWakeCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        let promise = runtime_native::rt_parallel_spawn_promise(
          parallel_worker_record_task,
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

extern "C" fn parallel_all_task(data: *mut u8, promise: PromiseRef) {
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
fn parallel_spawn_promise_can_be_awaited_from_coroutine() {
  let _rt = TestRuntimeGuard::new();

  let mut completed = false;
  let input = Box::new(41u32);
  let input_ptr = Box::into_raw(input);

  let coro_obj = unsafe { alloc_pinned::<AwaitParallelPromiseCoroutine>(RtShapeId(1)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: await_parallel_promise_resume,
    promise: PromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.completed = &mut completed;
  coro.input = input_ptr;

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
fn parallel_spawn_promise_rejection_can_be_awaited_from_coroutine() {
  let _rt = TestRuntimeGuard::new();

  let mut completed = false;
  let coro_obj = unsafe { alloc_pinned::<AwaitParallelRejectPromiseCoroutine>(RtShapeId(4)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: await_parallel_reject_promise_resume,
    promise: PromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.completed = &mut completed;

  runtime_native::rt_async_spawn_legacy(&mut coro.header);
  let start = Instant::now();
  while !completed {
    runtime_native::rt_async_poll_legacy();
    assert!(
      start.elapsed() < Duration::from_secs(2),
      "timeout waiting for parallel promise to settle"
    );
  }

  assert!(completed);
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

  let mut completed = false;
  let coro_obj = unsafe { alloc_pinned::<AwaitWorkerWakeCoroutine>(RtShapeId(2)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: await_worker_wake_resume,
    promise: PromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.completed = &mut completed;
  coro.data = data_ptr;

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

#[repr(C)]
struct WaitForStart {
  start: AtomicBool,
  work: AtomicUsize,
}

extern "C" fn wait_for_start_then_inc_and_fulfill(data: *mut u8, promise: PromiseRef) {
  // Safety: caller passed `Arc::into_raw(shared.clone()) as *mut u8`.
  let shared = unsafe { Arc::from_raw(data as *const WaitForStart) };
  while !shared.start.load(Ordering::Acquire) {
    std::thread::yield_now();
  }
  shared.work.fetch_add(1, Ordering::AcqRel);
  unsafe {
    runtime_native::rt_promise_fulfill(promise);
  }
}

#[test]
fn parallel_spawn_promise_wakes_blocked_async_poll() {
  let _rt = TestRuntimeGuard::new();
  let continuations = Arc::new(AtomicUsize::new(0));

  let shared = Arc::new(WaitForStart {
    start: AtomicBool::new(false),
    work: AtomicUsize::new(0),
  });

  let promise = runtime_native::rt_parallel_spawn_promise(
    wait_for_start_then_inc_and_fulfill,
    Arc::into_raw(shared.clone()) as *mut u8,
    PromiseLayout::of::<()>(),
  );
  runtime_native::rt_promise_then_legacy(
    promise,
    inc_atomic,
    Arc::into_raw(continuations.clone()) as *mut u8,
  );

  let (tx, rx) = mpsc::channel();
  // `rt_async_poll_legacy` can return spuriously (e.g. if the waker fd already has a pending
  // signal). Keep polling until the promise continuation has actually run so this test remains
  // deterministic under contention.
  let continuations_for_poll = continuations.clone();
  std::thread::spawn(move || {
    while continuations_for_poll.load(Ordering::Acquire) == 0 {
      runtime_native::rt_async_poll_legacy();
      std::thread::yield_now();
    }
    tx.send(()).unwrap();
  });

  // Wait for the event loop to actually block in `epoll_wait` (not just spin or return early).
  // Once blocked, release the worker so the promise fulfillment must wake the poll.
  let start = Instant::now();
  while !runtime_native::async_rt::debug_in_epoll_wait() {
    assert!(
      start.elapsed() < Duration::from_secs(2),
      "timeout waiting for rt_async_poll_legacy to block in epoll_wait"
    );
    std::thread::yield_now();
  }

  shared.start.store(true, Ordering::Release);
  rx.recv_timeout(Duration::from_secs(5))
    .expect("rt_async_poll_legacy did not wake after the parallel task completed");

  assert_eq!(shared.work.load(Ordering::Acquire), 1);
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
    let promise = runtime_native::rt_parallel_spawn_promise(
      parallel_all_task,
      i as *mut u8,
      PromiseLayout::of::<u32>(),
    );
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
  let coro_obj = unsafe { alloc_pinned::<AwaitAllCoroutine>(RtShapeId(3)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: await_all_resume,
    promise: PromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.completed = &mut completed;
  coro.awaited = all_promise;

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
