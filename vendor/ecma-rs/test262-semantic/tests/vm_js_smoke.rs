use conformance_harness::{Expectations, TimeoutManager};
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::sync::Mutex;
use std::time::Duration;
use tempfile::tempdir;
use test262_semantic::discover::discover_tests;
use test262_semantic::executor::default_executor;
use test262_semantic::harness::HarnessMode;
use test262_semantic::report::TestOutcome;
use test262_semantic::runner::{expand_cases, Filter};

static VM_JS_SMOKE_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn vm_js_executor_smoke_pass_and_timeout() {
  let _guard = VM_JS_SMOKE_LOCK.lock().unwrap();
  let temp = tempdir().unwrap();

  // Minimal fake test262 checkout: harness + test directories.
  fs::create_dir_all(temp.path().join("harness")).unwrap();
  fs::write(temp.path().join("harness/assert.js"), "").unwrap();
  fs::write(temp.path().join("harness/sta.js"), "").unwrap();

  let test_dir = temp.path().join("test");
  fs::create_dir_all(&test_dir).unwrap();

  // A tiny script that should execute successfully in both strict and non-strict modes.
  fs::write(
    test_dir.join("pass.js"),
    "/*---\nflags: []\n---*/\nvar x = 1;\n",
  )
  .unwrap();

  // A tight loop that should cooperatively time out via the shared interrupt flag.
  fs::write(
    test_dir.join("timeout.js"),
    "/*---\nflags: [onlyStrict]\n---*/\nwhile (true) {}\n",
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
    Duration::from_millis(100),
    &timeout_manager,
  );

  let mut by_id: HashMap<(&str, test262_semantic::report::Variant), &test262_semantic::report::TestResult> =
    HashMap::new();
  for result in &results {
    by_id.insert((result.id.as_str(), result.variant), result);
  }

  // pass.js generates both strict and non-strict variants.
  assert_eq!(
    by_id[&("pass.js", test262_semantic::report::Variant::NonStrict)].outcome,
    TestOutcome::Passed
  );
  assert_eq!(
    by_id[&("pass.js", test262_semantic::report::Variant::Strict)].outcome,
    TestOutcome::Passed
  );

  // timeout.js is onlyStrict and should deterministically time out.
  assert_eq!(
    by_id[&("timeout.js", test262_semantic::report::Variant::Strict)].outcome,
    TestOutcome::TimedOut
  );
}

#[test]
fn vm_js_executor_module_smoke_imports_and_harness_globals() {
  let _guard = VM_JS_SMOKE_LOCK.lock().unwrap();
  let temp = tempdir().unwrap();

  // Minimal fake test262 checkout: harness + test directories.
  fs::create_dir_all(temp.path().join("harness")).unwrap();
  // Intentionally define `assert` as a global binding (not `globalThis.assert`) so that the module
  // test can only see it if the harness prelude runs in the global realm.
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

  // Module dependency imported via a relative path.
  fs::write(test_dir.join("dep.js"), "export const y = 1;\n").unwrap();

  // Module test case that:
  // - imports `./dep.js` (exercises file-based module resolution), and
  // - calls `assert.sameValue` (ensures the harness prelude ran in the global realm).
  fs::write(
    test_dir.join("mod.js"),
    r#"/*---
flags: [module]
---*/
import { y } from "./dep.js";
assert.sameValue(y, 1);
"#,
  )
  .unwrap();

  let discovered = discover_tests(temp.path()).unwrap();
  let cases = expand_cases(
    &discovered,
    &Filter::Regex(Regex::new(r"^mod\.js$").unwrap()),
  )
  .unwrap();
  assert_eq!(cases.len(), 1);
  assert_eq!(cases[0].variant, test262_semantic::report::Variant::Module);

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
  let result = &results[0];
  assert_eq!(result.id, "mod.js");
  assert_eq!(result.variant, test262_semantic::report::Variant::Module);

  assert_eq!(
    result.outcome,
    TestOutcome::Passed,
    "expected module case to pass: {result:#?}"
  );
}
