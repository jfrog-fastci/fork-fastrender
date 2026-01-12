use std::fs;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use tempfile::tempdir;
use test262_semantic::discover::discover_tests;
use test262_semantic::executor::{default_executor, ExecError, ExecPhase};
use test262_semantic::harness::{assemble_source, HarnessMode};
use test262_semantic::report::Variant;
use test262_semantic::runner::{expand_cases, Filter};

#[test]
fn module_loader_rejects_sandbox_escape() {
  let temp = tempdir().unwrap();

  // Minimal fake test262 checkout: harness + test directories.
  fs::create_dir_all(temp.path().join("harness")).unwrap();
  fs::write(temp.path().join("harness/assert.js"), "").unwrap();
  fs::write(temp.path().join("harness/sta.js"), "").unwrap();

  // File outside the discovered `test/` tree. The loader must never read it.
  fs::write(temp.path().join("outside.js"), "export const SHOULD_NOT_LOAD = true;\n").unwrap();

  let test_dir = temp.path().join("test");
  fs::create_dir_all(&test_dir).unwrap();
  fs::write(
    test_dir.join("escape.js"),
    "/*---\nflags: [module]\n---*/\nimport '../outside.js';\n",
  )
  .unwrap();

  let discovered = discover_tests(temp.path()).unwrap();
  let cases = expand_cases(&discovered, &Filter::All).unwrap();
  let case = cases
    .iter()
    .find(|case| case.id == "escape.js")
    .expect("escape.js should be discovered");
  assert_eq!(case.variant, Variant::Module);

  let source = assemble_source(
    temp.path(),
    &case.metadata,
    case.variant,
    &case.body,
    HarnessMode::Test262,
  )
  .unwrap();

  let executor = default_executor();
  let cancel = Arc::new(AtomicBool::new(false));
  let err = executor.execute(case, &source, &cancel).unwrap_err();
  let ExecError::Js(js) = err else {
    panic!("expected JS error, got {err:?}");
  };

  assert_eq!(js.phase, ExecPhase::Resolution);
  assert_eq!(js.typ.as_deref(), Some("TypeError"));
  assert!(
    js.message.contains("sandbox") || js.message.contains("escapes"),
    "expected sandbox error message, got: {}",
    js.message
  );
}
