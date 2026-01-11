use std::process::Command;

use runtime_native::async_abi::{
  Coroutine, CoroutineStep, CoroutineVTable, PromiseHeader, RT_ASYNC_ABI_VERSION,
};
use runtime_native::RtShapeId;

unsafe extern "C" fn dummy_resume(_coro: *mut Coroutine) -> CoroutineStep {
  CoroutineStep::complete()
}

unsafe extern "C" fn dummy_destroy(_coro: *mut Coroutine) {}

#[test]
fn abi_version_mismatch_aborts() {
  let exe = std::env::current_exe().expect("current_exe");
  let output = Command::new(exe)
    .arg("--exact")
    .arg("abi_version_mismatch_child")
    .arg("--nocapture")
    .env("RT_ASYNC_ABI_MISMATCH_CHILD", "1")
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

#[test]
fn abi_version_mismatch_child() {
  if std::env::var_os("RT_ASYNC_ABI_MISMATCH_CHILD").is_none() {
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
    let _ = runtime_native::rt_async_spawn(&mut coro as *mut Coroutine);
  }
}

#[test]
fn reserved_nonzero_aborts() {
  let exe = std::env::current_exe().expect("current_exe");
  let output = Command::new(exe)
    .arg("--exact")
    .arg("reserved_nonzero_child")
    .arg("--nocapture")
    .env("RT_ASYNC_ABI_RESERVED_CHILD", "1")
    .output()
    .expect("spawn child test process");

  assert!(
    !output.status.success(),
    "expected child to abort (stdout: {}, stderr: {})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

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

#[test]
fn reserved_nonzero_child() {
  if std::env::var_os("RT_ASYNC_ABI_RESERVED_CHILD").is_none() {
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
    let _ = runtime_native::rt_async_spawn(&mut coro as *mut Coroutine);
  }
}

#[test]
fn promise_align_not_power_of_two_aborts() {
  let exe = std::env::current_exe().expect("current_exe");
  let output = Command::new(exe)
    .arg("--exact")
    .arg("promise_align_not_power_of_two_child")
    .arg("--nocapture")
    .env("RT_ASYNC_ABI_ALIGN_CHILD", "1")
    .output()
    .expect("spawn child test process");

  assert!(
    !output.status.success(),
    "expected child to abort (stdout: {}, stderr: {})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

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

#[test]
fn promise_align_not_power_of_two_child() {
  if std::env::var_os("RT_ASYNC_ABI_ALIGN_CHILD").is_none() {
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
    let _ = runtime_native::rt_async_spawn(&mut coro as *mut Coroutine);
  }
}

#[test]
fn promise_size_too_small_aborts() {
  let exe = std::env::current_exe().expect("current_exe");
  let output = Command::new(exe)
    .arg("--exact")
    .arg("promise_size_too_small_child")
    .arg("--nocapture")
    .env("RT_ASYNC_ABI_SIZE_CHILD", "1")
    .output()
    .expect("spawn child test process");

  assert!(
    !output.status.success(),
    "expected child to abort (stdout: {}, stderr: {})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

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

#[test]
fn promise_size_too_small_child() {
  if std::env::var_os("RT_ASYNC_ABI_SIZE_CHILD").is_none() {
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
    let _ = runtime_native::rt_async_spawn(&mut coro as *mut Coroutine);
  }
}

#[test]
fn promise_align_too_small_aborts() {
  let exe = std::env::current_exe().expect("current_exe");
  let output = Command::new(exe)
    .arg("--exact")
    .arg("promise_align_too_small_child")
    .arg("--nocapture")
    .env("RT_ASYNC_ABI_ALIGN_SMALL_CHILD", "1")
    .output()
    .expect("spawn child test process");

  assert!(
    !output.status.success(),
    "expected child to abort (stdout: {}, stderr: {})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

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

#[test]
fn promise_align_too_small_child() {
  if std::env::var_os("RT_ASYNC_ABI_ALIGN_SMALL_CHILD").is_none() {
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
    let _ = runtime_native::rt_async_spawn(&mut coro as *mut Coroutine);
  }
}
