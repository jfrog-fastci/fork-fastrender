use std::ffi::{CStr, CString};
use std::io;

// Seatbelt sandboxing is macOS-specific.
//
// We link to the system `libsandbox` to call `sandbox_init`, which installs a process-wide sandbox
// profile that cannot be reverted. Callers must apply it only once and must do so in a dedicated
// child process when running tests.
#[link(name = "sandbox")]
extern "C" {
  fn sandbox_init(
    profile: *const libc::c_char,
    flags: u64,
    errorbuf: *mut *mut libc::c_char,
  ) -> libc::c_int;
  fn sandbox_free_error(errorbuf: *mut libc::c_char);
}

// `sandbox_init` flags are not exposed in `libc`.
//
// Apple documents `SANDBOX_NAMED` as the flag to treat the `profile` string as a profile name
// rather than raw profile source code. We keep the constant local so the crate can compile on
// non-macOS targets without pulling in additional bindgen tooling.
const SANDBOX_NAMED: u64 = 0x0001;

fn sandbox_init_profile(profile: &CStr, flags: u64) -> io::Result<()> {
  let mut errorbuf: *mut libc::c_char = std::ptr::null_mut();

  // SAFETY: `sandbox_init` installs an irreversible process-wide sandbox. The FFI contract requires
  // a NUL-terminated profile string and a valid out-pointer for the error buffer.
  let rc = unsafe { sandbox_init(profile.as_ptr(), flags, &mut errorbuf) };
  if rc == 0 {
    return Ok(());
  }

  let message = if !errorbuf.is_null() {
    // SAFETY: `errorbuf` is allocated by `libsandbox` and is NUL-terminated.
    let message = unsafe { CStr::from_ptr(errorbuf) }
      .to_string_lossy()
      .into_owned();
    // SAFETY: `sandbox_free_error` frees the buffer allocated by `sandbox_init`.
    unsafe { sandbox_free_error(errorbuf) };
    message
  } else {
    "sandbox_init failed with unknown error".to_string()
  };

  Err(io::Error::new(io::ErrorKind::Other, message))
}

fn apply_named_profile(profile_name: &str) -> io::Result<()> {
  let profile_name =
    CString::new(profile_name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL"))?;
  sandbox_init_profile(&profile_name, SANDBOX_NAMED)
}

fn apply_profile_source(profile_source: &str) -> io::Result<()> {
  let profile_source = CString::new(profile_source)
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "sandbox profile contains NUL"))?;
  sandbox_init_profile(&profile_source, 0)
}

/// Apply the macOS Seatbelt "pure-computation" sandbox profile to the current process.
///
/// This profile is expected to prevent direct filesystem and network access.
///
/// ⚠️ This is irreversible for the lifetime of the process; tests must apply it in a dedicated
/// child process (see `FASTR_TEST_MACOS_SANDBOX_CHILD` in the unit tests below).
pub fn apply_pure_computation_sandbox() -> io::Result<()> {
  let named_err = match apply_named_profile("pure-computation") {
    Ok(()) => return Ok(()),
    Err(err) => err,
  };

  // Some environments may not support named profiles via `sandbox_init` (for example, if the flag
  // values differ across SDKs). Fall back to loading the profile source from known system paths.
  let candidate_paths = [
    "/usr/share/sandbox/pure-computation.sb",
    "/System/Library/Sandbox/Profiles/pure-computation.sb",
  ];
  for path in candidate_paths {
    if let Ok(source) = std::fs::read_to_string(path) {
      if let Ok(()) = apply_profile_source(&source) {
        return Ok(());
      }
    }
  }

  Err(named_err)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io;
  use std::net::{TcpListener, TcpStream};
  use std::process::Command;

  const CHILD_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_CHILD";
  const PORT_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_PORT";

  fn is_permission_error(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::PermissionDenied {
      return true;
    }
    matches!(
      err.raw_os_error(),
      Some(libc::EPERM) | Some(libc::EACCES)
    )
  }

  #[test]
  fn seatbelt_pure_computation_blocks_filesystem_and_network() {
    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      let port: u16 = std::env::var(PORT_ENV)
        .expect("child process missing sandbox port env var")
        .parse()
        .expect("parse sandbox port env var");

      apply_pure_computation_sandbox().expect("apply pure-computation sandbox");

      // 1) File read should fail.
      let read_err = std::fs::read_to_string("/private/etc/passwd")
        .or_else(|err| {
          if err.kind() == io::ErrorKind::NotFound {
            std::fs::read_to_string("/etc/passwd")
          } else {
            Err(err)
          }
        })
        .expect_err("expected filesystem read to be denied by sandbox");
      assert!(
        is_permission_error(&read_err),
        "expected file read to be denied by sandbox, got {read_err:?}"
      );

      // 2) File write should fail.
      let temp_path = std::env::temp_dir().join(format!(
        "fastr_sandbox_test_{}_write.txt",
        std::process::id()
      ));
      let write_err = std::fs::write(&temp_path, b"fastrender sandbox test")
        .expect_err("expected filesystem write to be denied by sandbox");
      assert!(
        is_permission_error(&write_err),
        "expected file write to be denied by sandbox, got {write_err:?}"
      );

      // 3) Network access should fail, even to localhost.
      let connect_err = TcpStream::connect(("127.0.0.1", port))
        .expect_err("expected network connect to be denied by sandbox");
      assert!(
        is_permission_error(&connect_err),
        "expected network connect to be denied by sandbox, got {connect_err:?}"
      );
      return;
    }

    let _listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test TCP listener");
    let port = _listener
      .local_addr()
      .expect("listener local addr")
      .port()
      .to_string();

    let exe = std::env::current_exe().expect("current test exe path");
    let test_name =
      "sandbox::macos::tests::seatbelt_pure_computation_blocks_filesystem_and_network";
    let output = Command::new(exe)
      .env(CHILD_ENV, "1")
      .env(PORT_ENV, port)
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
}

