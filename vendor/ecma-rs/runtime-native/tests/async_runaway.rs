use runtime_native::abi::{PromiseRef, RtCoroStatus, RtCoroutineHeader};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{
  rt_async_free_c_string,
  rt_async_poll_legacy as rt_async_poll,
  rt_async_set_limits,
  rt_async_spawn_legacy as rt_async_spawn,
  rt_async_take_last_error,
  rt_coro_await_legacy as rt_coro_await,
  rt_promise_new_legacy as rt_promise_new,
  rt_promise_resolve_legacy as rt_promise_resolve,
  set_strict_await_yields,
};
use std::ffi::CStr;

#[repr(C)]
struct RunawayCoro {
  header: RtCoroutineHeader,
  awaited: PromiseRef,
}

extern "C" fn runaway_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  // Safety: this is a test-only coroutine whose frame begins with RtCoroutineHeader.
  let coro = coro as *mut RunawayCoro;
  unsafe {
    rt_coro_await(&mut (*coro).header, (*coro).awaited, 1);
  }
  RtCoroStatus::Pending
}

#[test]
fn async_runaway_is_detected() {
  let _rt = TestRuntimeGuard::new();
  rt_async_set_limits(1_000, 100);
  set_strict_await_yields(true);

  let awaited = rt_promise_new();
  rt_promise_resolve(awaited, std::ptr::null_mut());

  let mut coro = Box::new(RunawayCoro {
    header: RtCoroutineHeader {
      resume: runaway_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: std::ptr::null_mut(),
      await_error: std::ptr::null_mut(),
    },
    awaited,
  });

  let _promise = rt_async_spawn(&mut coro.header);
  assert!(!rt_async_poll());

  let err_ptr = rt_async_take_last_error();
  assert!(!err_ptr.is_null());
  let err = unsafe { CStr::from_ptr(err_ptr) }.to_string_lossy().into_owned();
  unsafe { rt_async_free_c_string(err_ptr) };

  assert!(err.contains("async runaway"));
  assert!(err.contains("max_ready_steps_per_poll"));
}
