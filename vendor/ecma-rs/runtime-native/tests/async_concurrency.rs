use runtime_native::abi::{
  LegacyPromiseRef, PromiseRef, RtCoroStatus, RtCoroutineHeader, RtShapeDescriptor, RtShapeId, ValueRef,
};
use runtime_native::gc::ObjHeader;
use runtime_native::shape_table;
use runtime_native::test_util::TestRuntimeGuard;
use std::mem;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Once;
use std::time::Duration;

fn resolve_legacy_promise(p: PromiseRef, value: ValueRef) {
  runtime_native::rt_promise_resolve_legacy(p.0.cast(), value);
}

#[repr(C)]
struct GcBox<T> {
  header: ObjHeader,
  payload: T,
}

static SHAPE_TABLE_ONCE: Once = Once::new();
static EMPTY_PTR_OFFSETS: [u32; 0] = [];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
      size: mem::size_of::<GcBox<AwaitOnceCoroutine>>() as u32,
      align: 16,
      flags: 0,
      ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
      ptr_offsets_len: 0,
      reserved: 0,
    }];
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

unsafe fn alloc_pinned<T>(shape: RtShapeId) -> *mut GcBox<T> {
  ensure_shape_table();
  runtime_native::rt_alloc_pinned(mem::size_of::<GcBox<T>>(), shape).cast::<GcBox<T>>()
}

#[repr(C)]
struct AwaitOnceCoroutine {
  header: RtCoroutineHeader,
  counter: *const AtomicUsize,
  awaited: LegacyPromiseRef,
}

extern "C" fn resume_await_once(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut AwaitOnceCoroutine;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        runtime_native::rt_coro_await_legacy(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        assert_eq!((*coro).header.await_is_error, 0);
        assert_eq!((*coro).header.await_value as usize, 0xCAFE_BABE);

        (&*(*coro).counter).fetch_add(1, Ordering::SeqCst);
        runtime_native::rt_promise_resolve_legacy((*coro).header.promise, core::ptr::null_mut());
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn cross_thread_promise_resolve_wakes_waiter_via_rt_async_wait() {
  let _rt = TestRuntimeGuard::new();

  let awaited = runtime_native::rt_promise_new_legacy();
  let counter: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));

  let coro_obj = unsafe { alloc_pinned::<AwaitOnceCoroutine>(RtShapeId(1)) };
  let coro = unsafe { &mut (*coro_obj).payload };
  coro.header = RtCoroutineHeader {
    resume: resume_await_once,
    promise: core::ptr::null_mut(),
    state: 0,
    await_is_error: 0,
    await_value: core::ptr::null_mut(),
    await_error: core::ptr::null_mut(),
  };
  coro.counter = counter;
  coro.awaited = awaited;

  runtime_native::rt_async_spawn_legacy(&mut coro.header);
  assert_eq!(counter.load(Ordering::SeqCst), 0);

  let awaited_send = PromiseRef(awaited.cast());
  let resolver = std::thread::spawn(move || {
    std::thread::sleep(Duration::from_millis(50));
    resolve_legacy_promise(awaited_send, 0xCAFE_BABEusize as ValueRef);
  });

  runtime_native::rt_async_wait();
  resolver.join().unwrap();
  // `rt_async_wait` may return spuriously. After the resolver completes, drain the single-consumer
  // runtime so we don't miss work enqueued concurrently with the wait.
  while runtime_native::rt_async_poll_legacy() {}
  assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[test]
fn many_waiters_are_all_woken() {
  let _rt = TestRuntimeGuard::new();

  let awaited = runtime_native::rt_promise_new_legacy();
  let n = 128usize;
  let counter: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));

  for _ in 0..n {
    let coro_obj = unsafe { alloc_pinned::<AwaitOnceCoroutine>(RtShapeId(1)) };
    let coro = unsafe { &mut (*coro_obj).payload };
    coro.header = RtCoroutineHeader {
      resume: resume_await_once,
      promise: core::ptr::null_mut(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    };
    coro.counter = counter;
    coro.awaited = awaited;
    runtime_native::rt_async_spawn_legacy(&mut coro.header);
  }

  let awaited_send = PromiseRef(awaited.cast());
  let resolver = std::thread::spawn(move || {
    std::thread::sleep(Duration::from_millis(50));
    resolve_legacy_promise(awaited_send, 0xCAFE_BABEusize as ValueRef);
  });

  runtime_native::rt_async_wait();
  resolver.join().unwrap();
  // Drain after the resolver completes to handle spurious wakeups / late resolution.
  while runtime_native::rt_async_poll_legacy() {}
  assert_eq!(counter.load(Ordering::SeqCst), n);
}
