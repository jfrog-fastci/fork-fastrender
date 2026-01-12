use native_oracle_harness::fixtures::{
  discover_native_oracle_fixtures, run_expectation_suite, ExpectationSuiteOptions, FixtureKind,
};
use native_oracle_harness::run_fixture_ts_with_name;
use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/native_oracle")
}

#[test]
fn native_oracle_fixtures_pass() {
  let dir = fixture_dir();
  let cases: Vec<_> = discover_native_oracle_fixtures(&dir)
    .into_iter()
    .filter(|case| case.kind == FixtureKind::Observe)
    .collect();

  assert!(
    !cases.is_empty(),
    "expected at least one TS/TSX fixture under {}",
    dir.display()
  );

  let report = run_expectation_suite(
    &cases,
    |case| run_fixture_ts_with_name(&case.path.to_string_lossy(), &case.source),
    ExpectationSuiteOptions::default(),
  );

  if !report.is_success() {
    panic!("{}", report.render());
  }
}
