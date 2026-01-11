use runtime_native::abi::PromiseRef;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

#[repr(C)]
struct SleepTaskCtx {
  started: AtomicBool,
  done: AtomicBool,
}

extern "C" fn sleep_task(data: *mut u8, promise: PromiseRef) {
  let ctx = unsafe { &*(data as *const SleepTaskCtx) };
  ctx.started.store(true, Ordering::Release);
  std::thread::sleep(Duration::from_millis(300));
  runtime_native::rt_promise_resolve_legacy(promise, core::ptr::null_mut());
  ctx.done.store(true, Ordering::Release);
}

struct ResumeWorldOnDrop;

impl Drop for ResumeWorldOnDrop {
  fn drop(&mut self) {
    runtime_native::rt_gc_resume_world();
  }
}

#[test]
fn stop_the_world_completes_while_spawn_blocking_task_is_blocked() {
  let _rt = TestRuntimeGuard::new();
  threading::register_current_thread(ThreadKind::Main);

  let ctx = Box::new(SleepTaskCtx {
    started: AtomicBool::new(false),
    done: AtomicBool::new(false),
  });
  let ctx_ptr = Box::into_raw(ctx);

  let _promise = runtime_native::rt_spawn_blocking(sleep_task, ctx_ptr.cast::<u8>());

  let deadline = Instant::now() + Duration::from_secs(2);
  while !unsafe { &*ctx_ptr }.started.load(Ordering::Acquire) {
    assert!(Instant::now() < deadline, "spawn_blocking task did not start in time");
    std::thread::yield_now();
  }

  runtime_native::rt_gc_request_stop_the_world();
  let _resume = ResumeWorldOnDrop;
  assert!(
    runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_millis(100)),
    "world did not reach safepoint in time while spawn_blocking task was blocked"
  );
  runtime_native::rt_gc_resume_world();

  let deadline = Instant::now() + Duration::from_secs(2);
  while !unsafe { &*ctx_ptr }.done.load(Ordering::Acquire) {
    assert!(Instant::now() < deadline, "spawn_blocking task did not finish in time");
    std::thread::yield_now();
  }

  unsafe {
    drop(Box::from_raw(ctx_ptr));
  }

  threading::unregister_current_thread();
}

