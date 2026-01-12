use native_oracle_harness::{
  compare_native_against_vm_js_oracle, run_fixture_ts_outcome, run_fixture_ts_outcome_with_name_and_options,
  OracleHarnessOptions, RunOutcome, RunOutcomeCompareOptions, VmJsOracleRunner,
};

#[test]
fn run_outcome_ok() {
  let out = run_fixture_ts_outcome(r#"globalThis.__native_result = "ok";"#);
  match out {
    RunOutcome::Ok {
      value,
      stdout,
      stderr,
    } => {
      assert_eq!(value, "ok");
      assert_eq!(stdout, "");
      assert_eq!(stderr, "");
    }
    other => panic!("expected Ok, got {other:?}"),
  }
}

#[test]
fn run_outcome_throw() {
  let out = run_fixture_ts_outcome(r#"throw "boom";"#);
  match out {
    RunOutcome::Throw {
      message,
      stdout,
      stderr,
      ..
    } => {
      assert_eq!(message, "boom");
      assert_eq!(stdout, "");
      assert_eq!(stderr, "");
    }
    other => panic!("expected Throw, got {other:?}"),
  }
}

#[test]
fn run_outcome_terminated_out_of_fuel() {
  let mut opts = OracleHarnessOptions::default();
  opts.vm_options.default_fuel = Some(1_000);

  let out =
    run_fixture_ts_outcome_with_name_and_options("<fixture>", "while(true) {}", &opts);
  match out {
    RunOutcome::Terminated { message, .. } => {
      assert!(
        message.contains("out of fuel"),
        "expected termination to mention out of fuel, got {message:?}"
      );
    }
    other => panic!("expected Terminated, got {other:?}"),
  }
}

#[test]
fn run_outcome_compile_error() {
  let out = run_fixture_ts_outcome("enum E { A = 1 }");
  match out {
    RunOutcome::CompileError { diagnostic } => {
      assert_eq!(diagnostic.code.as_str(), "MINIFYTS0001");
    }
    other => panic!("expected CompileError, got {other:?}"),
  }
}

#[test]
fn compare_vm_js_oracle_runner_matches_itself() {
  let native = VmJsOracleRunner::new();
  compare_native_against_vm_js_oracle(&native, r#"globalThis.__native_result="ok";"#, RunOutcomeCompareOptions::default())
    .expect("oracle should match itself");
}

