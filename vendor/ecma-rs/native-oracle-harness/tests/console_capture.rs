use native_oracle_harness::{
  run_js_source_capture_stdout_with_options, run_typescript_source_capture_stdout_with_options,
  OracleHarnessOptions,
};

#[test]
fn console_log_capture_sync() {
  let out = run_js_source_capture_stdout_with_options(
    "<sync.js>",
    r#"console.log("ok");"#,
    &OracleHarnessOptions::default(),
  )
  .expect("script should run");
  assert_eq!(out, "ok");
}

#[test]
fn console_log_capture_microtasks() {
  let out = run_js_source_capture_stdout_with_options(
    "<microtasks.js>",
    r#"Promise.resolve().then(() => console.log("ok"));"#,
    &OracleHarnessOptions::default(),
  )
  .expect("script should run");
  assert_eq!(out, "ok");
}

#[test]
fn console_log_capture_ts() {
  let out = run_typescript_source_capture_stdout_with_options(
    "<ts.ts>",
    r#"console.log("ok");"#,
    &OracleHarnessOptions::default(),
  )
  .expect("script should run");
  assert_eq!(out, "ok");
}

