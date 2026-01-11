use runtime_native::test_util::TestRuntimeGuard;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Barrier;
use std::time::{Duration, Instant};

const CHILD_ENV: &str = "RUNTIME_NATIVE_ASYNC_DRIVER_GUARD_CHILD";

#[test]
fn rt_async_driver_concurrent_poll_aborts() {
  if std::env::var(CHILD_ENV).is_ok() {
    let _rt = TestRuntimeGuard::new();

    struct BlockCtx {
      entered: AtomicBool,
      barrier: Barrier,
    }

    extern "C" fn blocking_microtask(data: *mut u8) {
      let ctx = unsafe { &*(data as *const BlockCtx) };
      ctx.entered.store(true, Ordering::SeqCst);
      // Block forever (until the process aborts) so the async driver stays active.
      ctx.barrier.wait();
    }

    let ctx: &'static BlockCtx = Box::leak(Box::new(BlockCtx {
      entered: AtomicBool::new(false),
      barrier: Barrier::new(2),
    }));

    unsafe {
      runtime_native::rt_queue_microtask(runtime_native::abi::Microtask {
        func: blocking_microtask,
        data: ctx as *const BlockCtx as *mut u8,
        drop: None,
      });
    }

    // Drive until idle on another thread; it will block inside the microtask above.
    std::thread::spawn(|| {
      runtime_native::rt_async_run_until_idle();
    });

    // Wait until the driver thread is definitely executing user code (microtask).
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ctx.entered.load(Ordering::SeqCst) {
      assert!(Instant::now() < deadline, "timeout waiting for driver thread to enter microtask");
      std::thread::yield_now();
    }

    // Concurrent driver call from a different thread must abort the process.
    runtime_native::rt_async_poll();
    unreachable!("rt_async_poll should have aborted due to concurrent driver");
  }

  let exe = std::env::current_exe().expect("current_exe");
  let out = Command::new(exe)
    .arg("--exact")
    .arg("rt_async_driver_concurrent_poll_aborts")
    .env(CHILD_ENV, "1")
    .output()
    .expect("spawn child test process");

  assert!(
    !out.status.success(),
    "expected subprocess to abort\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
    out.status,
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr)
  );

  #[cfg(unix)]
  {
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(out.status.signal(), Some(libc::SIGABRT));
  }
}

#[test]
fn rt_async_run_until_idle_is_reentrant_noop() {
  let _rt = TestRuntimeGuard::new();

  struct Ctx {
    ran: AtomicBool,
    inner_returned_true: AtomicBool,
  }

  extern "C" fn reentrant_microtask(data: *mut u8) {
    let ctx = unsafe { &*(data as *const Ctx) };
    ctx.ran.store(true, Ordering::SeqCst);

    // Re-entrant driver call must be a no-op and return false (so we don't deadlock by
    // recursively driving the event loop).
    let res = runtime_native::rt_async_run_until_idle();
    ctx.inner_returned_true.store(res, Ordering::SeqCst);
  }

  let ctx: &'static Ctx = Box::leak(Box::new(Ctx {
    ran: AtomicBool::new(false),
    inner_returned_true: AtomicBool::new(false),
  }));

  unsafe {
    runtime_native::rt_queue_microtask(runtime_native::abi::Microtask {
      func: reentrant_microtask,
      data: ctx as *const Ctx as *mut u8,
      drop: None,
    });
  }

  assert!(runtime_native::rt_async_run_until_idle());
  assert!(ctx.ran.load(Ordering::SeqCst));
  assert!(
    !ctx.inner_returned_true.load(Ordering::SeqCst),
    "expected re-entrant rt_async_run_until_idle to return false"
  );
}

#[test]
fn rt_async_driver_concurrent_block_on_aborts() {
  if std::env::var(CHILD_ENV).is_ok() {
    let _rt = TestRuntimeGuard::new();

    struct HookCtx {
      entered: AtomicBool,
      barrier: Barrier,
    }

    let ctx: &'static HookCtx = Box::leak(Box::new(HookCtx {
      entered: AtomicBool::new(false),
      barrier: Barrier::new(2),
    }));

    // Install a hook that blocks at the end of the `rt_async_run_until_idle` call performed by
    // `rt_async_block_on`. This creates a deterministic window where:
    // - `rt_async_run_until_idle` itself has returned,
    // - but `rt_async_block_on` has not yet proceeded to its next step.
    //
    // `rt_async_block_on` must still hold the driver guard during this window.
    runtime_native::test_util::set_microtask_checkpoint_end_hook(Some(Box::new(move || {
      ctx.entered.store(true, Ordering::SeqCst);
      ctx.barrier.wait();
    })));

    let p = runtime_native::rt_promise_new_legacy();

    std::thread::spawn(move || unsafe {
      runtime_native::rt_async_block_on(p);
    });

    let deadline = Instant::now() + Duration::from_secs(2);
    while !ctx.entered.load(Ordering::SeqCst) {
      assert!(Instant::now() < deadline, "timeout waiting for block_on thread to enter hook");
      std::thread::yield_now();
    }

    // Concurrent driver call from a different thread must abort the process.
    runtime_native::rt_async_poll();
    unreachable!("rt_async_poll should have aborted due to concurrent driver (block_on active)");
  }

  let exe = std::env::current_exe().expect("current_exe");
  let out = Command::new(exe)
    .arg("--exact")
    .arg("rt_async_driver_concurrent_block_on_aborts")
    .env(CHILD_ENV, "1")
    .output()
    .expect("spawn child test process");

  assert!(
    !out.status.success(),
    "expected subprocess to abort\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
    out.status,
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr)
  );

  #[cfg(unix)]
  {
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(out.status.signal(), Some(libc::SIGABRT));
  }
}
