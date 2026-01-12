use native_oracle_harness::{
  run_fixture_ts_with_name_and_options, run_typescript_source_with_options, OracleHarnessError,
  OracleHarnessOptions,
};

#[test]
fn constrained_fuel_budget_terminates_consistently_across_protocols() {
  let mut opts = OracleHarnessOptions::default();
  opts.vm_options.default_fuel = Some(50);

  let src = "while(true){}";
  let source_name = "fuel_budget.ts";

  let err = run_typescript_source_with_options(source_name, src, &opts)
    .expect_err("promise-aware protocol should terminate");
  let promise_message = match err {
    OracleHarnessError::Terminated { message } => message,
    other => panic!("expected Terminated error, got {other:?}"),
  };
  assert!(
    promise_message.contains("out of fuel"),
    "expected out-of-fuel termination, got {promise_message:?}"
  );

  let diag = run_fixture_ts_with_name_and_options(source_name, src, &opts)
    .expect_err("observation protocol should terminate");
  assert!(
    diag.message.contains("out of fuel"),
    "expected out-of-fuel termination diagnostic, got {diag:?}"
  );
  assert_eq!(diag.message, promise_message);
}

