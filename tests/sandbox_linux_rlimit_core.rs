#![cfg(target_os = "linux")]

use std::io;
use std::process::Command;

fn get_rlimit_core() -> io::Result<(libc::rlim_t, libc::rlim_t)> {
  let mut current = libc::rlimit {
    rlim_cur: 0,
    rlim_max: 0,
  };
  // SAFETY: `getrlimit` writes to `current` while the pointer is valid.
  let rc = unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut current) };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }
  Ok((current.rlim_cur, current.rlim_max))
}

fn set_rlimit_core(cur: libc::rlim_t, max: libc::rlim_t) -> io::Result<()> {
  let new = libc::rlimit {
    rlim_cur: cur,
    rlim_max: max,
  };
  // SAFETY: `setrlimit` reads from a valid `rlimit` pointer for the duration of the syscall.
  let rc = unsafe { libc::setrlimit(libc::RLIMIT_CORE, &new) };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

fn run_child(test_name: &str, env_key: &str) {
  let exe = std::env::current_exe().expect("current_exe");
  let output = Command::new(exe)
    .arg("--exact")
    .arg(test_name)
    .arg("--nocapture")
    .env(env_key, "1")
    // Keep libtest single-threaded: sandboxing logic may opt into TSYNC in the future.
    .env("RUST_TEST_THREADS", "1")
    .output()
    .expect("spawn child test process");

  assert!(
    output.status.success(),
    "child process should exit successfully (stdout: {}, stderr: {})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}

fn is_child(env_key: &str) -> bool {
  std::env::var_os(env_key).is_some()
}

#[test]
fn sandbox_sets_rlimit_core_to_zero() {
  run_child(
    "sandbox_sets_rlimit_core_to_zero_child",
    "FASTR_SANDBOX_RLIMIT_CORE_CHILD",
  );
}

#[test]
fn sandbox_sets_rlimit_core_to_zero_child() {
  const ENV_KEY: &str = "FASTR_SANDBOX_RLIMIT_CORE_CHILD";
  if !is_child(ENV_KEY) {
    return;
  }

  let (_before_cur, before_max) =
    get_rlimit_core().expect("getrlimit(RLIMIT_CORE) before sandbox");

  // Many environments default to a non-zero/infinite core limit, but some (CI, containers) already
  // force core dumps off. If possible, set a non-zero limit so this test verifies that sandbox
  // application actively clamps it back to 0/0.
  if before_max > 0 {
    set_rlimit_core(1, 1).expect("setrlimit(RLIMIT_CORE, 1) before sandbox");
    let (cur, max) = get_rlimit_core().expect("getrlimit(RLIMIT_CORE) after pre-set");
    assert_eq!(cur, 1, "expected RLIMIT_CORE.cur to be 1 after pre-set");
    assert_eq!(max, 1, "expected RLIMIT_CORE.max to be 1 after pre-set");
  }

  fastrender::sandbox::apply_renderer_sandbox_prelude().expect("apply sandbox prelude");

  let (cur, max) = get_rlimit_core().expect("getrlimit(RLIMIT_CORE) after sandbox");
  assert_eq!(cur, 0, "expected RLIMIT_CORE.cur to be 0 after sandbox");
  assert_eq!(max, 0, "expected RLIMIT_CORE.max to be 0 after sandbox");
}
