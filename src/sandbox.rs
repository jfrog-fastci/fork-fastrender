use std::io;

/// Apply the macOS Seatbelt `pure-computation` sandbox to the current process.
///
/// This is intended for sandboxing untrusted renderer subprocesses. It is a one-way operation:
/// once applied, the sandbox cannot be removed.
pub fn apply_pure_computation_sandbox() -> io::Result<()> {
  #[cfg(target_os = "macos")]
  return macos::apply_named_sandbox("pure-computation");

  #[cfg(not(target_os = "macos"))]
  return Err(io::Error::new(
    io::ErrorKind::Unsupported,
    "Seatbelt sandboxing is only supported on macOS",
  ));
}

#[cfg(target_os = "macos")]
mod macos {
  use std::ffi::{CStr, CString};
  use std::io;
  use std::ptr;

  // From `<sandbox.h>`: treat the `profile` argument to `sandbox_init` as a named profile (e.g.
  // "pure-computation") instead of a raw profile source string.
  const SANDBOX_NAMED: u64 = 0x0001;

  #[link(name = "sandbox")]
  extern "C" {
    fn sandbox_init(
      profile: *const libc::c_char,
      flags: u64,
      errorbuf: *mut *mut libc::c_char,
    ) -> libc::c_int;
    fn sandbox_free_error(errorbuf: *mut libc::c_char);
  }

  pub(super) fn apply_named_sandbox(profile_name: &str) -> io::Result<()> {
    let profile_cstr = CString::new(profile_name).map_err(|_| {
      io::Error::new(
        io::ErrorKind::InvalidInput,
        "sandbox profile name contains NUL byte",
      )
    })?;

    let mut error_buf: *mut libc::c_char = ptr::null_mut();
    // SAFETY: The Seatbelt APIs are FFI calls. We pass valid pointers and free any returned error
    // string with `sandbox_free_error`.
    let rc = unsafe { sandbox_init(profile_cstr.as_ptr(), SANDBOX_NAMED, &mut error_buf) };
    if rc == 0 {
      return Ok(());
    }

    let msg = if error_buf.is_null() {
      format!("sandbox_init failed with rc={rc}")
    } else {
      // SAFETY: `error_buf` is a C string allocated by `sandbox_init`.
      let message = unsafe { CStr::from_ptr(error_buf) }
        .to_string_lossy()
        .into_owned();
      // SAFETY: `error_buf` came from `sandbox_init`.
      unsafe { sandbox_free_error(error_buf) };
      message
    };

    Err(io::Error::new(
      io::ErrorKind::Other,
      format!("failed to apply Seatbelt sandbox profile '{profile_name}': {msg}"),
    ))
  }
}

#[cfg(test)]
mod tests {
  #[cfg(target_os = "macos")]
  mod macos {
    use super::super::apply_pure_computation_sandbox;
    use std::io::Write;
    use std::process::Command;

    #[test]
    fn pure_computation_sandbox_allows_inherited_stdout_pipe() {
      const CHILD_ENV: &str = "FASTR_TEST_SANDBOX_STDOUT_CHILD";
      const SENTINEL: &[u8] = b"fastrender-seatbelt-stdout-ok";

      if std::env::var_os(CHILD_ENV).is_some() {
        apply_pure_computation_sandbox().expect("apply Seatbelt pure-computation sandbox");
        std::io::stdout()
          .write_all(SENTINEL)
          .and_then(|_| std::io::stdout().flush())
          .expect("write sentinel to stdout after sandbox");
        std::process::exit(0);
      }

      let exe = std::env::current_exe().expect("current test exe path");
      let test_name =
        "sandbox::tests::macos::pure_computation_sandbox_allows_inherited_stdout_pipe";
      let output = Command::new(exe)
        .env(CHILD_ENV, "1")
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .output()
        .expect("spawn sandbox child process");

      assert!(
        output.status.success(),
        "sandbox child should exit 0 (stdout={}, stderr={})",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
      );

      assert!(
        output
          .stdout
          .windows(SENTINEL.len())
          .any(|window| window == SENTINEL),
        "expected sandbox child to write sentinel to stdout; got stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
      );
    }
  }
}
