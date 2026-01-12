use native_oracle_harness::fixtures::{
  discover_native_oracle_fixtures, run_expectation_suite, ExpectationSuiteOptions, FixtureKind,
};
use native_oracle_harness::run_fixture_ts_with_name;
use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/native_oracle")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let dir = fixture_dir();
  let cases: Vec<_> = discover_native_oracle_fixtures(&dir)
    .into_iter()
    .filter(|case| case.kind == FixtureKind::Observe)
    .collect();

  if cases.is_empty() {
    return Err(format!("expected at least one TS/TSX fixture under {}", dir.display()).into());
  }

  let report = run_expectation_suite(
    &cases,
    |case| run_fixture_ts_with_name(&case.path.to_string_lossy(), &case.source),
    ExpectationSuiteOptions::default(),
  );

  for case in &cases {
    match report.failure_for_path(&case.path) {
      Some(failure) => eprintln!("{}", failure.render()),
      None => println!("ok {}", case.path.display()),
    }
  }

  if report.is_success() {
    Ok(())
  } else {
    Err(format!("{} fixture(s) failed", report.failed).into())
  }
}
