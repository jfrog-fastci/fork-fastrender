use assert_cmd::Command;

fn assert_aborts_with_callback_diagnostic(scenario: &str) {
  let mut cmd: Command = assert_cmd::cargo::cargo_bin_cmd!("ffi-panic-smoke");
  let output = cmd.arg(scenario).output().expect("failed to run ffi-panic-smoke");

  assert!(
    !output.status.success(),
    "expected scenario '{scenario}' to abort (success exit status); stderr:\n{}",
    String::from_utf8_lossy(&output.stderr)
  );

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("runtime-native: panic in callback"),
    "expected callback panic diagnostic; scenario='{scenario}'; stderr:\n{stderr}"
  );

  #[cfg(unix)]
  {
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
      output.status.signal(),
      Some(libc::SIGABRT),
      "expected abort signal for scenario '{scenario}'; status={:?}",
      output.status
    );
  }
}

#[test]
fn microtask_callback_panic_aborts() {
  assert_aborts_with_callback_diagnostic("microtask");
}

#[test]
fn parallel_callback_panic_aborts() {
  assert_aborts_with_callback_diagnostic("parallel");
}

#[test]
fn blocking_callback_panic_aborts() {
  assert_aborts_with_callback_diagnostic("blocking");
}
