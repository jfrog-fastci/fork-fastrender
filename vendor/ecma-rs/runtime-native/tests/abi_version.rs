use std::process::Command;

use runtime_native::async_abi::{
  Coroutine, CoroutineStep, CoroutineVTable, PromiseHeader, RT_ASYNC_ABI_VERSION,
};
use runtime_native::RtShapeId;

unsafe extern "C" fn dummy_resume(_coro: *mut Coroutine) -> CoroutineStep {
  CoroutineStep::complete()
}

unsafe extern "C" fn dummy_destroy(_coro: *mut Coroutine) {}

unsafe fn rt_async_spawn_ptr(coro: *mut Coroutine) {
  let coro_id = runtime_native::CoroutineId(runtime_native::rt_handle_alloc(coro.cast()));
  let _ = runtime_native::rt_async_spawn(coro_id);
}

fn run_abort_child(test_name: &str, env_key: &str) {
  let exe = std::env::current_exe().expect("current_exe");
  let output = Command::new(exe)
    .arg("--exact")
    .arg(test_name)
    .arg("--nocapture")
    .env(env_key, "1")
    .output()
    .expect("spawn child test process");

  assert!(
    !output.status.success(),
    "expected child to abort (stdout: {}, stderr: {})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  // `std::process::abort()` should terminate the process by signal on Unix,
  // rather than returning a normal exit code (e.g. panic exit code 101).
  #[cfg(unix)]
  {
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
      output.status.signal(),
      Some(libc::SIGABRT),
      "expected SIGABRT (stdout: {}, stderr: {})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }
}

fn is_child(env_key: &str) -> bool {
  std::env::var_os(env_key).is_some()
}

#[test]
fn abi_version_mismatch_aborts() {
  run_abort_child("abi_version_mismatch_child", "RT_ASYNC_ABI_MISMATCH_CHILD");
}

#[test]
fn abi_version_mismatch_child() {
  if !is_child("RT_ASYNC_ABI_MISMATCH_CHILD") {
    return;
  }

  // Construct a dummy coroutine header with a mismatched ABI version. The
  // runtime must detect this and abort deterministically rather than executing
  // with UB.
  static BAD_VTABLE: CoroutineVTable = CoroutineVTable {
    resume: dummy_resume,
    destroy: dummy_destroy,
    promise_size: core::mem::size_of::<PromiseHeader>() as u32,
    promise_align: core::mem::align_of::<PromiseHeader>() as u32,
    promise_shape_id: RtShapeId::INVALID,
    abi_version: RT_ASYNC_ABI_VERSION + 1,
    reserved: [0; 4],
  };

  let mut coro = Coroutine {
    vtable: &BAD_VTABLE,
    promise: core::ptr::null_mut(),
    next_waiter: core::ptr::null_mut(),
    flags: 0,
  };

  unsafe {
    rt_async_spawn_ptr(&mut coro as *mut Coroutine);
  }
}

#[test]
fn reserved_nonzero_aborts() {
  run_abort_child("reserved_nonzero_child", "RT_ASYNC_ABI_RESERVED_CHILD");
}

#[test]
fn reserved_nonzero_child() {
  if !is_child("RT_ASYNC_ABI_RESERVED_CHILD") {
    return;
  }

  static BAD_VTABLE: CoroutineVTable = CoroutineVTable {
    resume: dummy_resume,
    destroy: dummy_destroy,
    promise_size: core::mem::size_of::<PromiseHeader>() as u32,
    promise_align: core::mem::align_of::<PromiseHeader>() as u32,
    promise_shape_id: RtShapeId::INVALID,
    abi_version: RT_ASYNC_ABI_VERSION,
    reserved: [1, 0, 0, 0],
  };

  let mut coro = Coroutine {
    vtable: &BAD_VTABLE,
    promise: core::ptr::null_mut(),
    next_waiter: core::ptr::null_mut(),
    flags: 0,
  };

  unsafe {
    rt_async_spawn_ptr(&mut coro as *mut Coroutine);
  }
}

#[test]
fn promise_align_not_power_of_two_aborts() {
  run_abort_child("promise_align_not_power_of_two_child", "RT_ASYNC_ABI_ALIGN_CHILD");
}

#[test]
fn promise_align_not_power_of_two_child() {
  if !is_child("RT_ASYNC_ABI_ALIGN_CHILD") {
    return;
  }

  // 24 is >= PromiseHeader alignment (8) but not a power of two.
  static BAD_VTABLE: CoroutineVTable = CoroutineVTable {
    resume: dummy_resume,
    destroy: dummy_destroy,
    promise_size: core::mem::size_of::<PromiseHeader>() as u32,
    promise_align: 24,
    promise_shape_id: RtShapeId::INVALID,
    abi_version: RT_ASYNC_ABI_VERSION,
    reserved: [0; 4],
  };

  let mut coro = Coroutine {
    vtable: &BAD_VTABLE,
    promise: core::ptr::null_mut(),
    next_waiter: core::ptr::null_mut(),
    flags: 0,
  };

  unsafe {
    rt_async_spawn_ptr(&mut coro as *mut Coroutine);
  }
}

#[test]
fn promise_size_too_small_aborts() {
  run_abort_child("promise_size_too_small_child", "RT_ASYNC_ABI_SIZE_CHILD");
}

#[test]
fn promise_size_too_small_child() {
  if !is_child("RT_ASYNC_ABI_SIZE_CHILD") {
    return;
  }

  static BAD_VTABLE: CoroutineVTable = CoroutineVTable {
    resume: dummy_resume,
    destroy: dummy_destroy,
    promise_size: (core::mem::size_of::<PromiseHeader>() - 1) as u32,
    promise_align: core::mem::align_of::<PromiseHeader>() as u32,
    promise_shape_id: RtShapeId::INVALID,
    abi_version: RT_ASYNC_ABI_VERSION,
    reserved: [0; 4],
  };

  let mut coro = Coroutine {
    vtable: &BAD_VTABLE,
    promise: core::ptr::null_mut(),
    next_waiter: core::ptr::null_mut(),
    flags: 0,
  };

  unsafe {
    rt_async_spawn_ptr(&mut coro as *mut Coroutine);
  }
}

#[test]
fn promise_align_too_small_aborts() {
  run_abort_child(
    "promise_align_too_small_child",
    "RT_ASYNC_ABI_ALIGN_SMALL_CHILD",
  );
}

#[test]
fn promise_align_too_small_child() {
  if !is_child("RT_ASYNC_ABI_ALIGN_SMALL_CHILD") {
    return;
  }

  static BAD_VTABLE: CoroutineVTable = CoroutineVTable {
    resume: dummy_resume,
    destroy: dummy_destroy,
    promise_size: core::mem::size_of::<PromiseHeader>() as u32,
    // 4 is a power of two, but is < PromiseHeader alignment (8).
    promise_align: 4,
    promise_shape_id: RtShapeId::INVALID,
    abi_version: RT_ASYNC_ABI_VERSION,
    reserved: [0; 4],
  };

  let mut coro = Coroutine {
    vtable: &BAD_VTABLE,
    promise: core::ptr::null_mut(),
    next_waiter: core::ptr::null_mut(),
    flags: 0,
  };

  unsafe {
    rt_async_spawn_ptr(&mut coro as *mut Coroutine);
  }
}

#[repr(C)]
struct RawCoroutineVTable {
  resume: Option<unsafe extern "C" fn(*mut Coroutine) -> CoroutineStep>,
  destroy: Option<unsafe extern "C" fn(*mut Coroutine)>,
  promise_size: u32,
  promise_align: u32,
  promise_shape_id: RtShapeId,
  abi_version: u32,
  reserved: [usize; 4],
}

#[test]
fn resume_null_aborts() {
  run_abort_child("resume_null_child", "RT_ASYNC_ABI_RESUME_NULL_CHILD");
}

#[test]
fn resume_null_child() {
  if !is_child("RT_ASYNC_ABI_RESUME_NULL_CHILD") {
    return;
  }

  static BAD_VTABLE: RawCoroutineVTable = RawCoroutineVTable {
    resume: None,
    destroy: Some(dummy_destroy),
    promise_size: core::mem::size_of::<PromiseHeader>() as u32,
    promise_align: core::mem::align_of::<PromiseHeader>() as u32,
    promise_shape_id: RtShapeId::INVALID,
    abi_version: RT_ASYNC_ABI_VERSION,
    reserved: [0; 4],
  };

  let mut coro = Coroutine {
    vtable: &BAD_VTABLE as *const RawCoroutineVTable as *const CoroutineVTable,
    promise: core::ptr::null_mut(),
    next_waiter: core::ptr::null_mut(),
    flags: 0,
  };

  unsafe {
    rt_async_spawn_ptr(&mut coro as *mut Coroutine);
  }
}

#[test]
fn destroy_null_aborts() {
  run_abort_child("destroy_null_child", "RT_ASYNC_ABI_DESTROY_NULL_CHILD");
}

#[test]
fn destroy_null_child() {
  if !is_child("RT_ASYNC_ABI_DESTROY_NULL_CHILD") {
    return;
  }

  static BAD_VTABLE: RawCoroutineVTable = RawCoroutineVTable {
    resume: Some(dummy_resume),
    destroy: None,
    promise_size: core::mem::size_of::<PromiseHeader>() as u32,
    promise_align: core::mem::align_of::<PromiseHeader>() as u32,
    promise_shape_id: RtShapeId::INVALID,
    abi_version: RT_ASYNC_ABI_VERSION,
    reserved: [0; 4],
  };

  let mut coro = Coroutine {
    vtable: &BAD_VTABLE as *const RawCoroutineVTable as *const CoroutineVTable,
    promise: core::ptr::null_mut(),
    next_waiter: core::ptr::null_mut(),
    flags: 0,
  };

  unsafe {
    rt_async_spawn_ptr(&mut coro as *mut Coroutine);
  }
}
