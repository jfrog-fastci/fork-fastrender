use conformance_harness::{Expectations, TimeoutManager};
use std::fs;
use std::time::Duration;
use tempfile::tempdir;
use test262_semantic::discover::discover_tests;
use test262_semantic::executor::default_executor;
use test262_semantic::harness::HarnessMode;
use test262_semantic::report::{TestOutcome, Variant};
use test262_semantic::runner::{expand_cases, Filter};

#[test]
fn raw_flag_disables_harness_injection_even_in_test262_mode() {
  let temp = tempdir().unwrap();

  // Minimal fake test262 checkout: harness + test directories.
  fs::create_dir_all(temp.path().join("harness")).unwrap();
  fs::write(
    temp.path().join("harness/assert.js"),
    "var HARNESS_RAN = true;\n",
  )
  .unwrap();
  fs::write(temp.path().join("harness/sta.js"), "").unwrap();

  let test_dir = temp.path().join("test");
  fs::create_dir_all(&test_dir).unwrap();

  // A raw test should execute exactly once (non-strict) and must not have harness scripts
  // prepended. If `assert.js` is injected, `HARNESS_RAN` will be defined and this test will throw.
  fs::write(
    test_dir.join("raw.js"),
    "/*---\nflags: [raw]\n---*/\nif (typeof HARNESS_RAN !== 'undefined') { throw new Error('harness ran'); }\n",
  )
  .unwrap();

  let discovered = discover_tests(temp.path()).unwrap();
  let cases = expand_cases(&discovered, &Filter::All).unwrap();
  assert_eq!(cases.len(), 1, "raw test should expand to a single variant");
  assert_eq!(cases[0].variant, Variant::NonStrict);

  let expectations = Expectations::empty();
  let executor = default_executor();
  let timeout_manager = TimeoutManager::new();

  let results = test262_semantic::runner::run_cases(
    temp.path(),
    HarnessMode::Test262,
    &cases,
    &expectations,
    executor.as_ref(),
    Duration::from_secs(1),
    &timeout_manager,
  );

  assert_eq!(results.len(), 1);
  assert_eq!(
    results[0].outcome,
    TestOutcome::Passed,
    "raw test should not evaluate harness scripts: {:#?}",
    results[0]
  );
}

