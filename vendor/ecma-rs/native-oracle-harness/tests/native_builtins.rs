use native_oracle_harness::{
  run_js_source_with_native_builtins_capture_stdout, OracleHarnessError,
};

#[test]
fn print_captures_output() {
  let out = run_js_source_with_native_builtins_capture_stdout("<print>", "print(1, \"x\", true);")
    .expect("expected script to run");
  assert_eq!(out, "1 x true\n");
}

#[test]
fn console_log_captures_output() {
  let out = run_js_source_with_native_builtins_capture_stdout("<console>", "console.log(\"ok\");")
    .expect("expected script to run");
  assert_eq!(out, "ok\n");
}

#[test]
fn assert_throws() {
  let err =
    run_js_source_with_native_builtins_capture_stdout("<assert>", "assert(false, \"nope\");")
      .expect_err("expected script to throw");
  match err {
    OracleHarnessError::UncaughtException { message } => {
      assert!(
        message.contains("nope"),
        "expected error message to mention 'nope', got: {message:?}"
      );
    }
    other => panic!("expected UncaughtException, got {other:?}"),
  }
}

#[test]
fn microtask_print() {
  let out = run_js_source_with_native_builtins_capture_stdout(
    "<microtask>",
    "Promise.resolve().then(() => print(\"ok\"));",
  )
  .expect("expected script to run");
  assert_eq!(out, "ok\n");
}

