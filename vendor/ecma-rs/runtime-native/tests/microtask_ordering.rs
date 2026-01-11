use runtime_native::abi::{PromiseRef, RtCoroStatus, RtCoroutineHeader, ValueRef};
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::Mutex;

extern "C" fn log_a(data: *mut u8) {
  let log = unsafe { &*(data as *const Mutex<Vec<u8>>) };
  log.lock().unwrap().push(b'A');
}

extern "C" fn log_c(data: *mut u8) {
  let log = unsafe { &*(data as *const Mutex<Vec<u8>>) };
  log.lock().unwrap().push(b'C');
}

#[repr(C)]
struct AwaitCoro {
  header: RtCoroutineHeader,
  log: *const Mutex<Vec<u8>>,
  awaited: PromiseRef,
}

extern "C" fn await_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut AwaitCoro;
  assert!(!coro.is_null());

  unsafe {
    match (*coro).header.state {
      0 => {
        runtime_native::rt_coro_await_legacy(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        let log = &*(*coro).log;
        log.lock().unwrap().push(b'B');
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

fn drain_until_idle() {
  while runtime_native::rt_async_poll_legacy() {}
}

#[test]
fn queue_microtask_then_promise_wakeup_runs_in_fifo_order() {
  let _rt = TestRuntimeGuard::new();

  let log: &'static Mutex<Vec<u8>> = Box::leak(Box::new(Mutex::new(Vec::new())));
  let awaited = runtime_native::rt_promise_new_legacy();

  let mut coro = Box::new(AwaitCoro {
    header: RtCoroutineHeader {
      resume: await_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    log,
    awaited,
  });

  runtime_native::rt_async_spawn_legacy(&mut coro.header);

  runtime_native::rt_queue_microtask(log_a, (log as *const Mutex<Vec<u8>>).cast_mut().cast::<u8>());
  runtime_native::rt_promise_resolve_legacy(awaited, core::ptr::null_mut::<core::ffi::c_void>() as ValueRef);

  drain_until_idle();

  assert_eq!(&*log.lock().unwrap(), b"AB");
}

#[test]
fn promise_wakeup_then_queue_microtask_runs_in_fifo_order() {
  let _rt = TestRuntimeGuard::new();

  let log: &'static Mutex<Vec<u8>> = Box::leak(Box::new(Mutex::new(Vec::new())));
  let awaited = runtime_native::rt_promise_new_legacy();

  let mut coro = Box::new(AwaitCoro {
    header: RtCoroutineHeader {
      resume: await_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    log,
    awaited,
  });

  runtime_native::rt_async_spawn_legacy(&mut coro.header);

  runtime_native::rt_promise_resolve_legacy(awaited, core::ptr::null_mut::<core::ffi::c_void>() as ValueRef);
  runtime_native::rt_queue_microtask(log_a, (log as *const Mutex<Vec<u8>>).cast_mut().cast::<u8>());

  drain_until_idle();

  assert_eq!(&*log.lock().unwrap(), b"BA");
}

#[repr(C)]
struct ResolveCtx {
  log: *const Mutex<Vec<u8>>,
  awaited: PromiseRef,
}

extern "C" fn microtask_a_resolve_promise_and_queue_c(data: *mut u8) {
  let ctx = unsafe { &*(data as *const ResolveCtx) };
  let log = unsafe { &*ctx.log };

  log.lock().unwrap().push(b'A');
  runtime_native::rt_promise_resolve_legacy(ctx.awaited, core::ptr::null_mut::<core::ffi::c_void>() as ValueRef);
  runtime_native::rt_queue_microtask(log_c, (ctx.log as *const Mutex<Vec<u8>>).cast_mut().cast::<u8>());
}

#[test]
fn microtask_enqueues_coroutine_and_callback_in_fifo_order() {
  let _rt = TestRuntimeGuard::new();

  let log: &'static Mutex<Vec<u8>> = Box::leak(Box::new(Mutex::new(Vec::new())));
  let awaited = runtime_native::rt_promise_new_legacy();

  let mut coro = Box::new(AwaitCoro {
    header: RtCoroutineHeader {
      resume: await_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    log,
    awaited,
  });

  runtime_native::rt_async_spawn_legacy(&mut coro.header);

  let ctx: &'static ResolveCtx = Box::leak(Box::new(ResolveCtx { log, awaited }));
  runtime_native::rt_queue_microtask(
    microtask_a_resolve_promise_and_queue_c,
    (ctx as *const ResolveCtx).cast_mut().cast::<u8>(),
  );

  drain_until_idle();

  assert_eq!(&*log.lock().unwrap(), b"ABC");
}
