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
  let _guard = VM_JS_SMOKE_LOCK
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
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
  let (pass_cases, timeout_cases): (Vec<_>, Vec<_>) =
    cases.into_iter().partition(|c| c.id == "pass.js");

  let expectations = Expectations::empty();
  let executor = default_executor();
  let timeout_manager = TimeoutManager::new();

  // Run the passing cases with a generous timeout: this keeps the smoke test reliable on slower
  // machines without impacting runtime (they should finish well before the timeout).
  let mut results = test262_semantic::runner::run_cases(
    temp.path(),
    HarnessMode::Test262,
    &pass_cases,
    &expectations,
    executor.as_ref(),
    false,
    Duration::from_secs(2),
    &timeout_manager,
  );
  // Run the infinite-loop case with a short timeout so the test stays fast.
  results.extend(test262_semantic::runner::run_cases(
    temp.path(),
    HarnessMode::Test262,
    &timeout_cases,
    &expectations,
    executor.as_ref(),
    false,
    Duration::from_millis(100),
    &timeout_manager,
  ));

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
  let _guard = VM_JS_SMOKE_LOCK
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
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
  let sub_dir = test_dir.join("sub");
  fs::create_dir_all(&sub_dir).unwrap();

  // Module dependency imported via a relative path.
  //
  // It also calls `assert.sameValue` to ensure the harness prelude created a *global* `assert`
  // binding visible from all modules (not just the entry module).
  fs::write(
    sub_dir.join("dep2.js"),
    "assert.sameValue(1, 1);\nexport const z = 1;\n",
  )
  .unwrap();
  fs::write(
    sub_dir.join("dep.js"),
    "import { z } from './dep2.js';\nassert.sameValue(z, 1);\nexport const y = z;\n",
  )
  .unwrap();

  // Module test case that:
  // - imports `./dep.js` (exercises file-based module resolution), and
  // - calls `assert.sameValue` (ensures the harness prelude ran in the global realm).
  fs::write(
    test_dir.join("mod.js"),
    r#"/*---
flags: [module]
---*/
import { y } from "./sub/dep.js";
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
    false,
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

#[test]
fn vm_js_executor_module_import_meta_url_is_present() {
  let _guard = VM_JS_SMOKE_LOCK
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  let temp = tempdir().unwrap();

  // Minimal fake test262 checkout: harness + test directories.
  fs::create_dir_all(temp.path().join("harness")).unwrap();
  fs::write(temp.path().join("harness/assert.js"), "").unwrap();
  fs::write(temp.path().join("harness/sta.js"), "").unwrap();

  let test_dir = temp.path().join("test");
  fs::create_dir_all(&test_dir).unwrap();

  fs::write(
    test_dir.join("import_meta_url.js"),
    r#"/*---
flags: [module]
---*/
if (typeof import.meta.url !== "string") {
  throw new Error("expected import.meta.url to be a string, got: " + (typeof import.meta.url));
}
if (!import.meta.url.includes("import_meta_url.js")) {
  throw new Error("unexpected import.meta.url: " + import.meta.url);
}
"#,
  )
  .unwrap();

  let discovered = discover_tests(temp.path()).unwrap();
  let cases = expand_cases(
    &discovered,
    &Filter::Regex(Regex::new(r"^import_meta_url\.js$").unwrap()),
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
    false,
    Duration::from_secs(1),
    &timeout_manager,
  );

  assert_eq!(results.len(), 1);
  let result = &results[0];
  assert_eq!(result.id, "import_meta_url.js");
  assert_eq!(result.variant, test262_semantic::report::Variant::Module);
  assert_eq!(
    result.outcome,
    TestOutcome::Passed,
    "expected module case to pass: {result:#?}"
  );
}

#[test]
fn vm_js_executor_module_top_level_await_smoke() {
  let _guard = VM_JS_SMOKE_LOCK
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
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

  // Top-level await should resolve and allow the module to call $DONE.
  fs::write(
    test_dir.join("tla.js"),
    r#"/*---
flags: [module, async]
---*/
var x = await Promise.resolve(1);
assert.sameValue(x, 1);
$DONE();
"#,
  )
  .unwrap();

  let discovered = discover_tests(temp.path()).unwrap();
  let cases = expand_cases(&discovered, &Filter::Regex(Regex::new(r"^tla\.js$").unwrap())).unwrap();
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
    false,
    Duration::from_secs(1),
    &timeout_manager,
  );

  assert_eq!(results.len(), 1);
  let result = &results[0];
  assert_eq!(result.id, "tla.js");
  assert_eq!(result.variant, test262_semantic::report::Variant::Module);
  assert_eq!(
    result.outcome,
    TestOutcome::Passed,
    "expected module case to pass: {result:#?}"
  );
}

#[test]
fn vm_js_executor_module_var_initializer_smoke() {
  let _guard = VM_JS_SMOKE_LOCK
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
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

  // Regression test: `var` bindings are created during `ModuleDeclarationInstantiation`, so runtime
  // evaluation of `var` declarations must *not* throw `SyntaxError("Identifier has already been declared")`.
  fs::write(
    test_dir.join("var_init.js"),
    r#"/*---
flags: [module]
---*/
var x = 1;
assert.sameValue(x, 1);
"#,
  )
  .unwrap();

  let discovered = discover_tests(temp.path()).unwrap();
  let cases = expand_cases(
    &discovered,
    &Filter::Regex(Regex::new(r"^var_init\.js$").unwrap()),
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
    false,
    Duration::from_secs(1),
    &timeout_manager,
  );

  assert_eq!(results.len(), 1);
  let result = &results[0];
  assert_eq!(result.id, "var_init.js");
  assert_eq!(result.variant, test262_semantic::report::Variant::Module);
  assert_eq!(
    result.outcome,
    TestOutcome::Passed,
    "expected module case to pass: {result:#?}"
  );
}

#[test]
fn vm_js_executor_module_dynamic_import_of_async_module_smoke() {
  let _guard = VM_JS_SMOKE_LOCK
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
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

  fs::write(
    test_dir.join("dep.js"),
    r#"
await 1;
export default 42;
"#,
  )
  .unwrap();

  fs::write(
    test_dir.join("dyn_import.js"),
    r#"/*---
flags: [module, async]
---*/
import("./dep.js")
  .then(ns => assert.sameValue(ns.default, 42))
  .then($DONE, $DONE);
"#,
  )
  .unwrap();

  let discovered = discover_tests(temp.path()).unwrap();
  let cases =
    expand_cases(&discovered, &Filter::Regex(Regex::new(r"^dyn_import\.js$").unwrap())).unwrap();
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
    false,
    Duration::from_secs(1),
    &timeout_manager,
  );

  assert_eq!(results.len(), 1);
  let result = &results[0];
  assert_eq!(result.id, "dyn_import.js");
  assert_eq!(result.variant, test262_semantic::report::Variant::Module);
  assert_eq!(
    result.outcome,
    TestOutcome::Passed,
    "expected module case to pass: {result:#?}"
  );
}
