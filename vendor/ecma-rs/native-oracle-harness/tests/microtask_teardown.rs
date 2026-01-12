use std::path::PathBuf;

use native_oracle_harness::{run_fixture_with_options, OracleHarnessError, OracleHarnessOptions};

fn fixture_path(name: &str) -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("../fixtures/native_oracle")
    .join(name)
}

#[test]
fn microtask_leak_on_throw_does_not_panic() {
  let err = run_fixture_with_options(
    fixture_path("microtask_leak_on_throw.js"),
    &OracleHarnessOptions::default(),
  )
  .expect_err("fixture should fail");

  match err {
    OracleHarnessError::UncaughtException { message } => assert!(
      message.starts_with("boom"),
      "expected message to start with boom, got: {message}"
    ),
    other => panic!("expected UncaughtException, got {other:?}"),
  }
}

#[test]
fn stringify_value_tears_down_temporary_microtasks() {
  let err = run_fixture_with_options(
    fixture_path("stringify_value_enqueues_microtask.js"),
    &OracleHarnessOptions::default(),
  )
  .expect_err("fixture should fail");

  match err {
    OracleHarnessError::UncaughtException { message } => assert!(
      message.starts_with("boom"),
      "expected message to start with boom, got: {message}"
    ),
    other => panic!("expected UncaughtException, got {other:?}"),
  }
}

