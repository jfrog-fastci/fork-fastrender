use runtime_native::abi::{PromiseRef, RtCoroStatus, RtCoroutineHeader, RtShapeDescriptor, RtShapeId, ValueRef};
use runtime_native::gc::ObjHeader;
use runtime_native::shape_table;
use runtime_native::test_util::TestRuntimeGuard;
use std::mem;
use std::sync::{Mutex, Once};
use std::time::Duration;
use std::time::Instant;

#[repr(C)]
struct GcBox<T> {
  header: ObjHeader,
  payload: T,
}

static SHAPE_TABLE_ONCE: Once = Once::new();
static EMPTY_PTR_OFFSETS: [u32; 0] = [];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    // We treat test coroutines as GC leaf objects: they do not contain pointers into the GC heap.
    // This keeps the shape table minimal and avoids requiring tests to describe precise pointer maps.
    static SHAPES: [RtShapeDescriptor; 6] = [
      RtShapeDescriptor {
        size: mem::size_of::<GcBox<TestCoroutine>>() as u32,
        align: 16,
        flags: 0,
        ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: 0,
        reserved: 0,
      },
      RtShapeDescriptor {
        size: mem::size_of::<GcBox<OrderCoroutine>>() as u32,
        align: 16,
        flags: 0,
        ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: 0,
        reserved: 0,
      },
      RtShapeDescriptor {
        size: mem::size_of::<GcBox<SettledAwaitCoroutine>>() as u32,
        align: 16,
        flags: 0,
        ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: 0,
        reserved: 0,
      },
      RtShapeDescriptor {
        size: mem::size_of::<GcBox<SpawnBlockingCoroutine>>() as u32,
        align: 16,
        flags: 0,
        ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: 0,
        reserved: 0,
      },
      RtShapeDescriptor {
        size: mem::size_of::<GcBox<SpawnBlockingRejectCoroutine>>() as u32,
        align: 16,
        flags: 0,
        ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
        ptr_offsets_len: 0,
        reserved: 0,
      },
      RtShapeDescriptor {
        size: mem::size_of::<GcBox<YieldOnceCoroutine>>() as u32,
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

fn coro_rooted_in_runtime(obj: *mut u8) -> bool {
  runtime_native::threading::safepoint::with_world_stopped(|stop_epoch| {
    let mut found = false;
    runtime_native::threading::safepoint::for_each_root_slot_world_stopped(stop_epoch, |slot| {
      let value = unsafe { core::ptr::read_unaligned(slot) };
      if value == obj {
        found = true;
      }
    })
    .expect("root enumeration");
    found
  })
}

#[repr(C)]
struct TestCoroutine {
  header: RtCoroutineHeader,
  side_effect: *mut bool,
  completed: *mut bool,
  awaited: PromiseRef,
}

extern "C" fn test_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  // Safety: TestCoroutine is #[repr(C)] and RtCoroutineHeader is its first field.
  let coro = coro as *mut TestCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        *(*coro).side_effect = true;
        runtime_native::rt_coro_await_legacy(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        // The awaited promise settled and the runtime should have stored the result.
        assert_eq!((*coro).header.await_is_error, 0);
        assert_eq!((*coro).header.await_value as usize, 0xCAFE_BABE);

        *(*coro).completed = true;
        runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut::<core::ffi::c_void>());
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn coroutine_spawn_runs_sync_until_first_await_and_resumes_as_microtask() {
  let _rt = TestRuntimeGuard::new();
  let awaited = runtime_native::rt_promise_new_legacy();
  let mut side_effect = false;
  let mut completed = false;

  // Allocate the coroutine frame as a GC object. The legacy coroutine pointer passed to the runtime
  // is a derived pointer to the frame payload after the `ObjHeader` prefix (see `async_rt::coroutine`).
  let coro_obj = unsafe { alloc_pinned::<TestCoroutine>(RtShapeId(1)) };
  let coro_base = coro_obj.cast::<u8>();
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: test_resume,
    promise: PromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.side_effect = &mut side_effect;
  coro.completed = &mut completed;
  coro.awaited = awaited;

  let promise = runtime_native::rt_async_spawn_legacy(&mut coro.header);

  // JS semantics: the coroutine runs immediately until its first `await`.
  assert!(side_effect);
  assert!(!completed);
  assert_eq!(promise, coro.header.promise);

  assert!(
    coro_rooted_in_runtime(coro_base),
    "await-suspended coroutine frame must be registered as a GC root"
  );

  // Settling the awaited promise should enqueue a microtask, not resume immediately.
  runtime_native::rt_promise_resolve_legacy(awaited, 0xCAFE_BABE as ValueRef);
  assert!(!completed);

  while runtime_native::rt_async_poll_legacy() {}

  assert!(completed);
}

#[repr(C)]
struct OrderCoroutine {
  header: RtCoroutineHeader,
  id: u32,
  log: *const Mutex<Vec<u32>>,
  awaited: PromiseRef,
}

extern "C" fn order_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut OrderCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        runtime_native::rt_coro_await_legacy(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        let log = &*(*coro).log;
        log.lock().unwrap().push((*coro).id);
        runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut::<core::ffi::c_void>());
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn promise_waiters_resume_in_fifo_order() {
  let _rt = TestRuntimeGuard::new();

  let awaited = runtime_native::rt_promise_new_legacy();
  let log: &'static Mutex<Vec<u32>> = Box::leak(Box::new(Mutex::new(Vec::new())));

  let coro1_obj = unsafe { alloc_pinned::<OrderCoroutine>(RtShapeId(2)) };
  let coro2_obj = unsafe { alloc_pinned::<OrderCoroutine>(RtShapeId(2)) };
  let coro1 = unsafe { &mut (*coro1_obj).payload };
  let coro2 = unsafe { &mut (*coro2_obj).payload };

  coro1.header = RtCoroutineHeader {
    resume: order_resume,
    promise: PromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro1.id = 1;
  coro1.log = log;
  coro1.awaited = awaited;

  coro2.header = RtCoroutineHeader {
    resume: order_resume,
    promise: PromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro2.id = 2;
  coro2.log = log;
  coro2.awaited = awaited;

  runtime_native::rt_async_spawn_legacy(&mut coro1.header);
  runtime_native::rt_async_spawn_legacy(&mut coro2.header);

  runtime_native::rt_promise_resolve_legacy(awaited, 0x1234usize as ValueRef);
  while runtime_native::rt_async_poll_legacy() {}

  assert_eq!(&*log.lock().unwrap(), &[1, 2]);
}

#[repr(C)]
struct SettledAwaitCoroutine {
  header: RtCoroutineHeader,
  completed: *mut bool,
  awaited: PromiseRef,
}

extern "C" fn settled_await_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut SettledAwaitCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        runtime_native::rt_coro_await_legacy(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        *(*coro).completed = true;
        runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut::<core::ffi::c_void>());
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn strict_mode_awaiting_settled_promise_yields_to_microtask() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::rt_async_set_strict_await_yields(true);

  let awaited = runtime_native::rt_promise_new_legacy();
  runtime_native::rt_promise_resolve_legacy(awaited, 0xBEEFusize as ValueRef);

  let mut completed = false;
  let coro_obj = unsafe { alloc_pinned::<SettledAwaitCoroutine>(RtShapeId(3)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: settled_await_resume,
    promise: PromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.completed = &mut completed;
  coro.awaited = awaited;

  runtime_native::rt_async_spawn_legacy(&mut coro.header);
  assert!(!completed, "strict await should not resume synchronously inside rt_async_spawn");

  runtime_native::rt_async_poll_legacy();
  assert!(completed);
  assert!(!runtime_native::rt_async_poll_legacy());
}

#[test]
fn non_strict_mode_awaiting_settled_promise_resumes_synchronously() {
  let _rt = TestRuntimeGuard::new();
  runtime_native::rt_async_set_strict_await_yields(false);

  let awaited = runtime_native::rt_promise_new_legacy();
  runtime_native::rt_promise_resolve_legacy(awaited, 0xBEEFusize as ValueRef);

  let mut completed = false;
  let coro_obj = unsafe { alloc_pinned::<SettledAwaitCoroutine>(RtShapeId(3)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: settled_await_resume,
    promise: PromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.completed = &mut completed;
  coro.awaited = awaited;

  runtime_native::rt_async_spawn_legacy(&mut coro.header);
  assert!(completed, "non-strict await should resume synchronously inside rt_async_spawn");
  assert!(!runtime_native::rt_async_poll_legacy());
}

// -----------------------------------------------------------------------------
// Yield rooting (macrotask scheduling)
// -----------------------------------------------------------------------------

#[repr(C)]
struct YieldOnceCoroutine {
  header: RtCoroutineHeader,
  yielded: *mut bool,
  completed: *mut bool,
}

extern "C" fn yield_once_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut YieldOnceCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        *(*coro).yielded = true;
        (*coro).header.state = 1;
        RtCoroStatus::Yield
      }
      1 => {
        *(*coro).completed = true;
        runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut::<core::ffi::c_void>());
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn coroutine_yield_is_rooted_while_enqueued_as_macrotask() {
  let _rt = TestRuntimeGuard::new();

  let mut yielded = false;
  let mut completed = false;

  let coro_obj = unsafe { alloc_pinned::<YieldOnceCoroutine>(RtShapeId(6)) };
  let coro_base = coro_obj.cast::<u8>();
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: yield_once_resume,
    promise: PromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.yielded = &mut yielded;
  coro.completed = &mut completed;

  runtime_native::rt_async_spawn_legacy(&mut coro.header);
  assert!(yielded);
  assert!(!completed);

  assert!(
    coro_rooted_in_runtime(coro_base),
    "yielded coroutine must be rooted while queued in the macrotask queue"
  );

  while runtime_native::rt_async_poll_legacy() {}

  assert!(completed);
  assert!(
    !coro_rooted_in_runtime(coro_base),
    "completed coroutine should not remain rooted by the runtime queues"
  );
}

// -----------------------------------------------------------------------------
// spawn_blocking integration
// -----------------------------------------------------------------------------

extern "C" fn blocking_resolve_value(_data: *mut u8, promise: PromiseRef) {
  std::thread::sleep(Duration::from_millis(20));
  runtime_native::rt_promise_resolve_legacy(promise, 0xCAFE_BABEusize as ValueRef);
}

extern "C" fn blocking_reject_value(_data: *mut u8, promise: PromiseRef) {
  std::thread::sleep(Duration::from_millis(20));
  runtime_native::rt_promise_reject_legacy(promise, 0xDEAD_BEEFusize as ValueRef);
}

#[repr(C)]
struct SpawnBlockingCoroutine {
  header: RtCoroutineHeader,
  completed: *mut bool,
  awaited: PromiseRef,
}

extern "C" fn spawn_blocking_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut SpawnBlockingCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        (*coro).awaited = runtime_native::rt_spawn_blocking(blocking_resolve_value, core::ptr::null_mut());
        runtime_native::rt_coro_await_legacy(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        assert_eq!((*coro).header.await_is_error, 0);
        assert_eq!((*coro).header.await_value as usize, 0xCAFE_BABE);
        *(*coro).completed = true;
        runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut::<core::ffi::c_void>());
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn coroutine_can_await_spawn_blocking_promise() {
  let _rt = TestRuntimeGuard::new();

  let mut completed = false;
  let coro_obj = unsafe { alloc_pinned::<SpawnBlockingCoroutine>(RtShapeId(4)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: spawn_blocking_resume,
    promise: PromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.completed = &mut completed;
  coro.awaited = PromiseRef::null();

  runtime_native::rt_async_spawn_legacy(&mut coro.header);
  assert!(
    !completed,
    "spawn_blocking should not resume synchronously inside rt_async_spawn when the promise is pending"
  );

  let start = Instant::now();
  while !completed {
    runtime_native::rt_async_poll_legacy();
    assert!(
      start.elapsed() < Duration::from_secs(2),
      "timeout waiting for spawn_blocking promise to resume coroutine"
    );
    std::thread::yield_now();
  }
}

#[repr(C)]
struct SpawnBlockingRejectCoroutine {
  header: RtCoroutineHeader,
  completed: *mut bool,
  awaited: PromiseRef,
}

extern "C" fn spawn_blocking_reject_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut SpawnBlockingRejectCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        (*coro).awaited = runtime_native::rt_spawn_blocking(blocking_reject_value, core::ptr::null_mut());
        runtime_native::rt_coro_await_legacy(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        assert_eq!((*coro).header.await_is_error, 1);
        assert_eq!((*coro).header.await_error as usize, 0xDEAD_BEEF);
        *(*coro).completed = true;
        runtime_native::rt_promise_resolve_legacy(
          (*coro).header.promise,
          core::ptr::null_mut::<core::ffi::c_void>(),
        );
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn coroutine_can_await_spawn_blocking_rejection() {
  let _rt = TestRuntimeGuard::new();

  let mut completed = false;
  let coro_obj = unsafe { alloc_pinned::<SpawnBlockingRejectCoroutine>(RtShapeId(5)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: spawn_blocking_reject_resume,
    promise: PromiseRef::null(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.completed = &mut completed;
  coro.awaited = PromiseRef::null();

  runtime_native::rt_async_spawn_legacy(&mut coro.header);
  assert!(!completed);

  let start = Instant::now();
  while !completed {
    runtime_native::rt_async_poll_legacy();
    assert!(
      start.elapsed() < Duration::from_secs(2),
      "timeout waiting for spawn_blocking rejection to resume coroutine"
    );
    std::thread::yield_now();
  }
}
