use native_oracle_harness::{
  run_fixture_ts_outcome, run_js_source_capture_stdout_with_options, OracleHarnessOptions, RunOutcome,
};

#[test]
fn print_captures_output() {
  let out = run_js_source_capture_stdout_with_options(
    "<print.js>",
    r#"print(1, "x", true);"#,
    &OracleHarnessOptions::default(),
  )
  .expect("script should run");
  assert_eq!(out, "1 x true");
}

#[test]
fn assert_throws() {
  let out = run_fixture_ts_outcome(r#"assert(false, "nope");"#);
  match out {
    RunOutcome::Throw { message, .. } => assert!(
      message.contains("nope"),
      "expected throw message to contain 'nope', got {message:?}"
    ),
    other => panic!("expected Throw, got {other:?}"),
  }
}

#[test]
fn panic_throws() {
  let out = run_fixture_ts_outcome(r#"panic("boom");"#);
  match out {
    RunOutcome::Throw { message, .. } => assert!(
      message.contains("boom"),
      "expected throw message to contain 'boom', got {message:?}"
    ),
    other => panic!("expected Throw, got {other:?}"),
  }
}

#[test]
fn microtask_print() {
  let out = run_js_source_capture_stdout_with_options(
    "<microtask_print.js>",
    r#"Promise.resolve().then(() => print("ok"));"#,
    &OracleHarnessOptions::default(),
  )
  .expect("script should run");
  assert_eq!(out, "ok");
}

