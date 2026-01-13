use conformance_harness::{Expectations, TimeoutManager};
use regex::Regex;
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
fn script_variants_support_dynamic_import() {
  let temp = tempdir().unwrap();

  // Minimal fake test262 checkout: harness + test directories.
  fs::create_dir_all(temp.path().join("harness")).unwrap();
  fs::write(
    temp.path().join("harness/assert.js"),
    r#"
var assert = {};
assert.sameValue = function (actual, expected) {
  if (actual !== expected) {
    throw new Error("assert.sameValue failed: expected " + expected + ", got " + actual);
  }
};
"#,
  )
  .unwrap();
  fs::write(temp.path().join("harness/sta.js"), "").unwrap();

  let test_dir = temp.path().join("test");
  fs::create_dir_all(&test_dir).unwrap();

  // Module dependency imported via dynamic `import()` from a classic script.
  fs::write(test_dir.join("dep.js"), "export const x = 1;\n").unwrap();

  // Script test case (no `flags: [module]`): this should be expanded into both strict and non-strict
  // variants, and `import()` should resolve using the file-based module loader.
  fs::write(
    test_dir.join("dynamic_import_script.js"),
    r#"/*---
flags: []
---*/
import('./dep.js').then(ns => { assert.sameValue(ns.x, 1); });
"#,
  )
  .unwrap();

  let discovered = discover_tests(temp.path()).unwrap();
  let cases = expand_cases(
    &discovered,
    &Filter::Regex(Regex::new(r"^dynamic_import_script\.js$").unwrap()),
  )
  .unwrap();
  assert_eq!(
    cases.iter().map(|c| c.variant).collect::<Vec<_>>(),
    vec![Variant::NonStrict, Variant::Strict],
    "expected script test to expand into strict + non-strict variants"
  );

  let expectations = Expectations::empty();
  let executor = default_executor();
  let timeout_manager = TimeoutManager::new();

  let results = test262_semantic::runner::run_cases(
    temp.path(),
    HarnessMode::Test262,
    &cases,
    &expectations,
    executor.as_ref(),
    false,
    Duration::from_secs(1),
    &timeout_manager,
  );

  let by_variant: HashMap<Variant, &test262_semantic::report::TestResult> =
    results.iter().map(|r| (r.variant, r)).collect();

  assert_eq!(by_variant[&Variant::NonStrict].outcome, TestOutcome::Passed);
  assert_eq!(by_variant[&Variant::Strict].outcome, TestOutcome::Passed);
}
