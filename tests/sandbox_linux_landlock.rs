#![cfg(target_os = "linux")]

use std::io;
use std::process::Command;

use fastrender::sandbox::linux_landlock;

#[test]
fn landlock_deny_all_blocks_etc_passwd() {
  const CHILD_ENV: &str = "FASTR_TEST_LANDLOCK_CHILD";
  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    if !std::path::Path::new("/etc/passwd").exists() {
      eprintln!("skipping Landlock test: /etc/passwd does not exist on this system");
      return;
    }

    let status =
      linux_landlock::apply(&linux_landlock::LandlockConfig::deny_all()).expect("apply landlock");
    match status {
      linux_landlock::LandlockStatus::Unsupported { reason } => {
        eprintln!("skipping Landlock test: Landlock unsupported ({reason:?})");
        return;
      }
      linux_landlock::LandlockStatus::Applied { abi } => {
        eprintln!("Landlock applied (abi={abi})");
      }
    }

    let path = std::ffi::CString::new("/etc/passwd").unwrap();
    // SAFETY: `path` is a NUL-terminated string.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd >= 0 {
      // SAFETY: `fd` is a valid file descriptor.
      unsafe { libc::close(fd) };
      panic!("expected open(/etc/passwd) to be denied by Landlock");
    }
    let errno = io::Error::last_os_error()
      .raw_os_error()
      .expect("errno should be set");
    assert!(
      errno == libc::EPERM || errno == libc::EACCES,
      "expected permission error (EPERM/EACCES) when opening /etc/passwd under deny-all landlock, got {errno}"
    );
    return;
  }

  let exe = std::env::current_exe().expect("current test exe path");
  let test_name = "landlock_deny_all_blocks_etc_passwd";
  let output = Command::new(exe)
    .env(CHILD_ENV, "1")
    .arg("--exact")
    .arg(test_name)
    .arg("--nocapture")
    .output()
    .expect("spawn child test process");
  assert!(
    output.status.success(),
    "child process should exit successfully (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}
