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
    eprintln!(
      "usage: ffi-panic-smoke <microtask|parallel|parallel-promise|blocking|thenable|native-async>"
    );
    std::process::exit(2);
  };

  match scenario.as_str() {
    "microtask" => scenario_microtask(),
    "parallel" => scenario_parallel(),
    "parallel-promise" => scenario_parallel_promise(),
    "blocking" => scenario_blocking(),
    "thenable" => scenario_thenable(),
    "native-async" => scenario_native_async(),
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
      drop: None,
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

fn scenario_parallel_promise() {
  use runtime_native::async_runtime::PromiseLayout;

  extern "C-unwind" fn panic_task(_data: *mut u8, _promise: runtime_native::PromiseRef) {
    panic!("intentional panic from parallel spawn_promise callback");
  }

  let cb: extern "C" fn(*mut u8, runtime_native::PromiseRef) = unsafe {
    std::mem::transmute(
      panic_task as extern "C-unwind" fn(*mut u8, runtime_native::PromiseRef),
    )
  };
  let _promise =
    runtime_native::rt_parallel_spawn_promise(cb, std::ptr::null_mut(), PromiseLayout::of::<u64>());

  // Keep the main thread alive until the worker thread aborts the process.
  std::thread::park();
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

fn scenario_thenable() {
  unsafe extern "C-unwind" fn panic_call_then(
    _thenable: *mut u8,
    _on_fulfilled: runtime_native::abi::ThenableResolveCallback,
    _on_rejected: runtime_native::abi::ThenableRejectCallback,
    _data: *mut u8,
  ) -> runtime_native::abi::ValueRef {
    panic!("intentional panic from thenable vtable call_then");
  }

  let call_then: unsafe extern "C" fn(
    thenable: *mut u8,
    on_fulfilled: runtime_native::abi::ThenableResolveCallback,
    on_rejected: runtime_native::abi::ThenableRejectCallback,
    data: *mut u8,
  ) -> runtime_native::abi::ValueRef = unsafe {
    std::mem::transmute(
      panic_call_then
        as unsafe extern "C-unwind" fn(
          *mut u8,
          runtime_native::abi::ThenableResolveCallback,
          runtime_native::abi::ThenableRejectCallback,
          *mut u8,
        ) -> runtime_native::abi::ValueRef,
    )
  };

  let vtable = runtime_native::abi::ThenableVTable { call_then };
  let thenable = runtime_native::abi::ThenableRef {
    vtable: std::ptr::from_ref(&vtable),
    ptr: std::ptr::null_mut(),
  };

  let p = runtime_native::rt_promise_new_legacy();
  runtime_native::rt_promise_resolve_thenable_legacy(p, thenable);
  let _ = runtime_native::rt_async_poll_legacy();
}

fn scenario_native_async() {
  use runtime_native::async_abi::{Coroutine, CoroutineRef, CoroutineStep, CoroutineVTable};

  unsafe extern "C-unwind" fn panic_resume(_coro: CoroutineRef) -> CoroutineStep {
    panic!("intentional panic from async_abi CoroutineVTable.resume");
  }

  unsafe extern "C-unwind" fn destroy(coro: CoroutineRef) {
    if coro.is_null() {
      return;
    }
    unsafe {
      drop(Box::from_raw(coro));
    }
  }

  let vtable = CoroutineVTable {
    resume: unsafe { std::mem::transmute(panic_resume as unsafe extern "C-unwind" fn(CoroutineRef) -> CoroutineStep) },
    destroy: unsafe { std::mem::transmute(destroy as unsafe extern "C-unwind" fn(CoroutineRef)) },
    promise_size: core::mem::size_of::<runtime_native::async_abi::PromiseHeader>() as u32,
    promise_align: core::mem::align_of::<runtime_native::async_abi::PromiseHeader>() as u32,
    promise_shape_id: runtime_native::RtShapeId(1),
    abi_version: runtime_native::async_abi::RT_ASYNC_ABI_VERSION,
    reserved: [0; 4],
  };

  let coro = Box::new(Coroutine {
    vtable: std::ptr::from_ref(&vtable),
    promise: core::ptr::null_mut(),
    next_waiter: core::ptr::null_mut(),
    flags: runtime_native::async_abi::CORO_FLAG_RUNTIME_OWNS_FRAME,
  });
  let coro_ptr: CoroutineRef = Box::into_raw(coro);

  let handle = runtime_native::rt_handle_alloc(coro_ptr.cast::<u8>());
  let _promise = unsafe { runtime_native::rt_async_spawn_deferred(runtime_native::CoroutineId(handle)) };
  let _ = runtime_native::rt_async_poll();

  // Keep the main thread alive until the scheduled resume aborts the process.
  std::thread::park();
}
