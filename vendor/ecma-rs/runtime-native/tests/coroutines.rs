use runtime_native::abi::{PromiseRef, RtCoroStatus, RtCoroutineHeader, ValueRef};
use runtime_native::test_util::TestRuntimeGuard;

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
        runtime_native::rt_coro_await(&mut (*coro).header, (*coro).awaited, 1);
        RtCoroStatus::Pending
      }
      1 => {
        // The awaited promise settled and the runtime should have stored the result.
        assert_eq!((*coro).header.await_is_error, 0);
        assert_eq!((*coro).header.await_value as usize, 0xCAFE_BABE);

        *(*coro).completed = true;
        runtime_native::rt_promise_resolve((*coro).header.promise, core::ptr::null_mut::<core::ffi::c_void>());
        RtCoroStatus::Done
      }
      other => panic!("unexpected coroutine state: {other}"),
    }
  }
}

#[test]
fn coroutine_spawn_runs_sync_until_first_await_and_resumes_as_microtask() {
  let _rt = TestRuntimeGuard::new();
  let awaited = runtime_native::rt_promise_new();
  let mut side_effect = false;
  let mut completed = false;

  let mut coro = Box::new(TestCoroutine {
    header: RtCoroutineHeader {
      resume: test_resume,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    side_effect: &mut side_effect,
    completed: &mut completed,
    awaited,
  });

  let promise = runtime_native::rt_async_spawn(&mut coro.header);

  // JS semantics: the coroutine runs immediately until its first `await`.
  assert!(side_effect);
  assert!(!completed);
  assert_eq!(promise, coro.header.promise);

  // Settling the awaited promise should enqueue a microtask, not resume immediately.
  runtime_native::rt_promise_resolve(awaited, 0xCAFE_BABE as ValueRef);
  assert!(!completed);

  while runtime_native::rt_async_poll() {}

  assert!(completed);
}
