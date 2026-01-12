use runtime_native::abi::{LegacyPromiseRef, RtCoroStatus, RtCoroutineHeader};
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{
  rt_async_poll_legacy as rt_async_poll,
  rt_async_set_limits,
  rt_async_spawn_legacy as rt_async_spawn,
  rt_async_take_last_error,
  rt_coro_await_legacy as rt_coro_await,
  rt_promise_new_legacy as rt_promise_new,
  rt_promise_resolve_legacy as rt_promise_resolve,
  set_strict_await_yields,
};

#[repr(C)]
struct CountedCoro {
  header: RtCoroutineHeader,
  awaited: LegacyPromiseRef,
  remaining: u32,
  resumes: u32,
}

extern "C" fn counted_resume(coro: *mut RtCoroutineHeader) -> RtCoroStatus {
  let coro = coro as *mut CountedCoro;
  unsafe {
    (*coro).resumes += 1;
    if (*coro).remaining == 0 {
      rt_promise_resolve(PromiseRef((*coro).header.promise.cast()), std::ptr::null_mut());
      return RtCoroStatus::Done;
    }
    (*coro).remaining -= 1;
    rt_coro_await(&mut (*coro).header, (*coro).awaited, 1);
    RtCoroStatus::Pending
  }
}

#[test]
fn normal_async_workload_completes() {
  let _rt = TestRuntimeGuard::new();
  rt_async_set_limits(10_000, 100);
  set_strict_await_yields(true);

  let awaited = rt_promise_new();
  rt_promise_resolve(awaited, std::ptr::null_mut());

  let mut coro = Box::new(CountedCoro {
    header: RtCoroutineHeader {
      resume: counted_resume,
      promise: std::ptr::null_mut(),
      state: 0,
      await_is_error: 0,
      await_value: std::ptr::null_mut(),
      await_error: std::ptr::null_mut(),
    },
    awaited,
    remaining: 10,
    resumes: 0,
  });

  let _promise = rt_async_spawn(&mut coro.header);
  assert!(!rt_async_poll());

  assert_eq!(coro.resumes, 11);
  assert!(rt_async_take_last_error().is_null());
}
