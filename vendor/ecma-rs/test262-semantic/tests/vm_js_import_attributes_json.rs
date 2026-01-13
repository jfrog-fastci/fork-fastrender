use conformance_harness::{Expectations, TimeoutManager};
use std::collections::HashMap;
use std::fs;
use std::time::Duration;
use tempfile::tempdir;
use test262_semantic::discover::discover_tests;
use test262_semantic::executor::default_executor;
use test262_semantic::harness::HarnessMode;
use test262_semantic::report::{TestOutcome, Variant};
use test262_semantic::runner::{expand_cases, Filter};

#[test]
fn import_attributes_json_modules() {
  let temp = tempdir().unwrap();

  // Minimal fake test262 checkout: harness + test directories.
  fs::create_dir_all(temp.path().join("harness")).unwrap();
  fs::write(
    temp.path().join("harness/assert.js"),
    r#"
var assert = {};
assert.sameValue = function (actual, expected, message) {
  if (actual !== expected) {
    throw new Test262Error(message || ("Expected SameValue"));
  }
};
"#,
  )
  .unwrap();
  fs::write(
    temp.path().join("harness/sta.js"),
    r#"
function Test262Error(message) {
  this.message = message || "";
}
Test262Error.prototype.toString = function () {
  return "Test262Error: " + this.message;
};
"#,
  )
  .unwrap();

  let test_dir = temp.path().join("test");
  fs::create_dir_all(&test_dir).unwrap();

  // A simple JSON module import that should succeed.
  fs::write(
    test_dir.join("json-pass.js"),
    "/*---\nflags: [module]\n---*/\nimport value from './data.json' with { type: 'json' };\nassert.sameValue(value, true);\n",
  )
  .unwrap();
  fs::write(test_dir.join("data.json"), "true").unwrap();

  // Invalid JSON should throw SyntaxError during module resolution.
  fs::write(
    test_dir.join("json-bad.js"),
    "/*---\nflags: [module]\nnegative:\n  phase: resolution\n  type: SyntaxError\n---*/\nimport value from './bad.json' with { type: 'json' };\n",
  )
  .unwrap();
  fs::write(test_dir.join("bad.json"), "{").unwrap();

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

  let mut by_id: HashMap<(&str, Variant), &test262_semantic::report::TestResult> = HashMap::new();
  for result in &results {
    by_id.insert((result.id.as_str(), result.variant), result);
  }

  let pass = by_id[&("json-pass.js", Variant::Module)];
  assert_eq!(pass.outcome, TestOutcome::Passed);
  assert!(!pass.mismatched);

  let bad = by_id[&("json-bad.js", Variant::Module)];
  assert_eq!(bad.outcome, TestOutcome::Passed);
  assert!(
    !bad.mismatched,
    "negative expectation mismatch; error was: {:#?}",
    bad
  );
}
