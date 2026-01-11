use std::path::PathBuf;

use native_oracle_harness::{run_fixture_with_options, OracleHarnessError, OracleHarnessOptions};

fn fixture_path(name: &str) -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("../fixtures/native_oracle")
    .join(name)
}

#[test]
fn await_promise_resolve_returns_ok() {
  let out = run_fixture_with_options(fixture_path("await_promise_resolve.js"), &OracleHarnessOptions::default())
    .expect("fixture should run");
  assert_eq!(out, "ok");
}

#[test]
fn promise_all_preserves_input_order() {
  let out = run_fixture_with_options(fixture_path("promise_all_order.js"), &OracleHarnessOptions::default())
    .expect("fixture should run");
  assert_eq!(out, "ab");
}

#[test]
fn promise_all_preserves_input_order_out_of_order_resolution() {
  let out = run_fixture_with_options(
    fixture_path("promise_all_out_of_order.js"),
    &OracleHarnessOptions::default(),
  )
  .expect("fixture should run");
  assert_eq!(out, "ab");
}

#[test]
fn promise_rejection_is_reported() {
  let err = run_fixture_with_options(fixture_path("promise_reject.js"), &OracleHarnessOptions::default())
    .expect_err("fixture should fail");
  match err {
    OracleHarnessError::PromiseRejected { reason } => assert_eq!(reason, "nope"),
    other => panic!("expected PromiseRejected, got {other:?}"),
  }
}

#[test]
fn pending_promise_does_not_hang() {
  let mut opts = OracleHarnessOptions::default();
  opts.max_microtask_checkpoints = 8;

  let err = run_fixture_with_options(fixture_path("promise_pending.js"), &opts).expect_err("fixture should fail");
  match err {
    OracleHarnessError::PromiseDidNotSettle { .. } => {}
    other => panic!("expected PromiseDidNotSettle, got {other:?}"),
  }
}
