use std::io;

#[cfg(target_os = "macos")]
pub mod macos;

/// Apply the macOS Seatbelt `pure-computation` sandbox to the current process.
///
/// This is intended for sandboxing untrusted renderer subprocesses. It is a one-way operation:
/// once applied, the sandbox cannot be removed.
pub fn apply_pure_computation_sandbox() -> io::Result<()> {
  #[cfg(target_os = "macos")]
  return macos::apply_pure_computation_sandbox();

  #[cfg(not(target_os = "macos"))]
  return Err(io::Error::new(
    io::ErrorKind::Unsupported,
    "Seatbelt sandboxing is only supported on macOS",
  ));
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
        "expected sandbox child to write sentinel to stdout; got stdout={}, stderr={} ",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
      );
    }
  }
}
