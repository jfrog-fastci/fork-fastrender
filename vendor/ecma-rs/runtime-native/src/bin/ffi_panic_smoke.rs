//! Subprocess smoke tests for runtime-native's FFI panic/unwind boundaries.
//!
//! This binary is exercised by `tests/ffi_panic_smoke.rs` and is expected to
//! abort (SIGABRT) after printing `runtime-native: panic in callback` when a
//! callback panics.

use std::io::Write;
use std::time::Duration;

fn main() {
  // Avoid hanging the test runner if something goes wrong and we *don't* abort.
  std::thread::spawn(|| {
    std::thread::sleep(Duration::from_secs(5));
    let _ = std::io::stderr().write_all(b"ffi-panic-smoke: timed out waiting for abort\n");
    std::process::exit(1);
  });

  let Some(scenario) = std::env::args().nth(1) else {
    eprintln!("usage: ffi-panic-smoke <microtask|parallel|blocking>");
    std::process::exit(2);
  };

  match scenario.as_str() {
    "microtask" => scenario_microtask(),
    "parallel" => scenario_parallel(),
    "blocking" => scenario_blocking(),
    other => {
      eprintln!("unknown scenario: {other}");
      std::process::exit(2);
    }
  }

  // If we return, then the runtime did not abort as expected.
  eprintln!("ffi-panic-smoke: scenario '{scenario}' completed without abort");
}

fn scenario_microtask() {
  extern "C-unwind" fn panic_cb(_data: *mut u8) {
    panic!("intentional panic from microtask callback");
  }

  // `runtime-native`'s exported ABI uses `extern \"C\"` function pointers.
  // Transmute so the callback can unwind and be caught by the runtime.
  let cb: extern "C" fn(*mut u8) = unsafe { std::mem::transmute(panic_cb as extern "C-unwind" fn(*mut u8)) };
  unsafe {
    runtime_native::rt_queue_microtask(runtime_native::abi::Microtask {
      func: cb,
      data: std::ptr::null_mut(),
    });
  }

  let _ = runtime_native::rt_async_poll_legacy();
}

fn scenario_parallel() {
  extern "C-unwind" fn panic_task(_data: *mut u8) {
    panic!("intentional panic from parallel task callback");
  }

  let cb: extern "C" fn(*mut u8) = unsafe { std::mem::transmute(panic_task as extern "C-unwind" fn(*mut u8)) };
  let id = runtime_native::rt_parallel_spawn(cb, std::ptr::null_mut());
  runtime_native::rt_parallel_join(&id as *const _, 1);
}

fn scenario_blocking() {
  extern "C-unwind" fn panic_task(_data: *mut u8, _promise: runtime_native::PromiseRef) {
    panic!("intentional panic from blocking pool task callback");
  }

  let cb: extern "C" fn(*mut u8, runtime_native::PromiseRef) =
    unsafe { std::mem::transmute(panic_task as extern "C-unwind" fn(*mut u8, runtime_native::PromiseRef)) };
  let _promise = runtime_native::rt_spawn_blocking(cb, std::ptr::null_mut());

  // Keep the main thread alive until the worker thread aborts the process.
  std::thread::park();
}
