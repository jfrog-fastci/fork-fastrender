use conformance_harness::{Expectations, TimeoutManager};
use std::fs;
use std::time::Duration;
use tempfile::tempdir;
use test262_semantic::discover::discover_tests;
use test262_semantic::executor::default_executor;
use test262_semantic::harness::HarnessMode;
use test262_semantic::report::{TestOutcome, Variant};
use test262_semantic::runner::{expand_cases, Filter};

fn write_minimal_harness(test262_dir: &std::path::Path) {
  let harness_dir = test262_dir.join("harness");
  fs::create_dir_all(&harness_dir).unwrap();
  fs::write(
    harness_dir.join("assert.js"),
    r#"
function assert(condition, message) {
  if (!condition) {
    throw new Error(message || "Assertion failed");
  }
}
assert.sameValue = function(actual, expected, message) {
  if (actual !== expected) {
    throw new Error(message || ("Expected " + actual + " === " + expected));
  }
};
"#,
  )
  .unwrap();
  // Included by default in `HarnessMode::Test262`; keep it empty for these tests.
  fs::write(harness_dir.join("sta.js"), "").unwrap();
}

#[test]
fn async_module_test_waits_for_done_and_passes() {
  let temp = tempdir().unwrap();
  write_minimal_harness(temp.path());

  let test_dir = temp.path().join("test");
  fs::create_dir_all(&test_dir).unwrap();

  fs::write(
    test_dir.join("async-module-pass.js"),
    r#"/*---
flags: [module, async]
---*/
Promise.resolve()
  .then(() => assert.sameValue(1, 1))
  .then($DONE, $DONE);
"#,
  )
  .unwrap();

  let discovered = discover_tests(temp.path()).unwrap();
  let cases = expand_cases(&discovered, &Filter::All).unwrap();

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

  let result = results
    .iter()
    .find(|r| r.id == "async-module-pass.js" && r.variant == Variant::Module)
    .unwrap();
  assert_eq!(result.outcome, TestOutcome::Passed);
}

#[test]
fn async_module_test_done_error_is_reported() {
  let temp = tempdir().unwrap();
  write_minimal_harness(temp.path());

  let test_dir = temp.path().join("test");
  fs::create_dir_all(&test_dir).unwrap();

  fs::write(
    test_dir.join("async-module-fail.js"),
    r#"/*---
flags: [module, async]
---*/
Promise.resolve().then(() => $DONE(new Error("boom")));
"#,
  )
  .unwrap();

  let discovered = discover_tests(temp.path()).unwrap();
  let cases = expand_cases(&discovered, &Filter::All).unwrap();

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

  let result = results
    .iter()
    .find(|r| r.id == "async-module-fail.js" && r.variant == Variant::Module)
    .unwrap();
  assert_eq!(result.outcome, TestOutcome::Failed);
  let err = result.error.as_deref().unwrap_or("");
  assert!(
    err.contains("Error: boom"),
    "expected error message to include error name/message, got: {err}"
  );
}

