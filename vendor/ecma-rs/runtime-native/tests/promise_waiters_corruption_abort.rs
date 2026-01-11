use std::process::Command;
use std::sync::atomic::Ordering;

use runtime_native::async_abi::PromiseHeader;
use runtime_native::PromiseRef;

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
fn promise_waiters_corruption_aborts() {
  run_abort_child(
    "promise_waiters_corruption_child",
    "RT_PROMISE_WAITERS_CORRUPTION_CHILD",
  );
}

#[test]
fn promise_waiters_corruption_child() {
  if !is_child("RT_PROMISE_WAITERS_CORRUPTION_CHILD") {
    return;
  }

  // Mimic a compiler-allocated native async-ABI promise: a `PromiseHeader` prefix at offset 0.
  let layout = std::alloc::Layout::from_size_align(
    core::mem::size_of::<PromiseHeader>(),
    core::mem::align_of::<PromiseHeader>(),
  )
  .expect("PromiseHeader layout");
  let p = unsafe { std::alloc::alloc_zeroed(layout) };
  assert!(!p.is_null(), "alloc_zeroed failed");

  let p_ref = PromiseRef(p.cast());
  unsafe {
    runtime_native::rt_promise_init(p_ref);
  }

  // Corrupt the waiter list head with a misaligned sentinel (historically `1` was reserved). All
  // runtime waiter registration + drain codepaths should detect this and abort deterministically
  // rather than dereferencing the value as a pointer (UB / segfault).
  let header = p.cast::<PromiseHeader>();
  unsafe {
    (*header).waiters.store(1, Ordering::Release);
    let _ = runtime_native::rt_promise_try_fulfill(p_ref);
  }
}

