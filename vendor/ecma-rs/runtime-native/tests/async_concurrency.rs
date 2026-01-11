use runtime_native::abi::{PromiseRef, RtCoroStatus, RtCoroutineHeader, ValueRef};
use runtime_native::test_util::TestRuntimeGuard;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[repr(C)]
struct AwaitOnceCoroutine {
  header: RtCoroutineHeader,
  counter: *const AtomicUsize,
  awaited: PromiseRef,
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

  let mut coro = Box::new(AwaitOnceCoroutine {
    header: RtCoroutineHeader {
      resume: resume_await_once,
      promise: PromiseRef::null(),
      state: 0,
      await_is_error: 0,
      await_value: core::ptr::null_mut(),
      await_error: core::ptr::null_mut(),
    },
    counter,
    awaited,
  });

  runtime_native::rt_async_spawn_legacy(&mut coro.header);
  assert_eq!(counter.load(Ordering::SeqCst), 0);

  let resolver = std::thread::spawn(move || {
    std::thread::sleep(Duration::from_millis(50));
    runtime_native::rt_promise_resolve_legacy(awaited, 0xCAFE_BABEusize as ValueRef);
  });

  runtime_native::rt_async_wait();

  while runtime_native::rt_async_poll_legacy() {}

  resolver.join().unwrap();
  assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[test]
fn many_waiters_are_all_woken() {
  let _rt = TestRuntimeGuard::new();

  let awaited = runtime_native::rt_promise_new_legacy();
  let n = 128usize;
  let counter: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));

  let mut coros = Vec::with_capacity(n);
  for _ in 0..n {
    let mut coro = Box::new(AwaitOnceCoroutine {
      header: RtCoroutineHeader {
        resume: resume_await_once,
        promise: PromiseRef::null(),
        state: 0,
        await_is_error: 0,
        await_value: core::ptr::null_mut(),
        await_error: core::ptr::null_mut(),
      },
      counter,
      awaited,
    });
    runtime_native::rt_async_spawn_legacy(&mut coro.header);
    coros.push(coro);
  }

  let resolver = std::thread::spawn(move || {
    std::thread::sleep(Duration::from_millis(50));
    runtime_native::rt_promise_resolve_legacy(awaited, 0xCAFE_BABEusize as ValueRef);
  });

  runtime_native::rt_async_wait();

  resolver.join().unwrap();
  while runtime_native::rt_async_poll_legacy() {}
  assert_eq!(counter.load(Ordering::SeqCst), n);

  drop(coros);
}
