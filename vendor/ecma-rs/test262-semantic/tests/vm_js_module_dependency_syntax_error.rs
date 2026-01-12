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
fn module_dependency_syntax_error_is_resolution_syntaxerror() {
  let temp = tempdir().unwrap();

  // Minimal fake test262 checkout: harness + test directories.
  fs::create_dir_all(temp.path().join("harness")).unwrap();
  fs::write(temp.path().join("harness/assert.js"), "").unwrap();
  fs::write(temp.path().join("harness/sta.js"), "").unwrap();

  let test_dir = temp.path().join("test");
  fs::create_dir_all(&test_dir).unwrap();

  // Entry module imports a dependency that fails to parse.
  fs::write(
    test_dir.join("main.js"),
    "/*---\nflags: [module]\n---*/\nimport './bad.js';\n",
  )
  .unwrap();

  // A simple syntax error.
  fs::write(test_dir.join("bad.js"), "break;\n").unwrap();

  // Discover only the entry test case (the dependency is not a test262 test).
  let discovered = discover_tests(temp.path()).unwrap();
  let discovered: Vec<_> = discovered.into_iter().filter(|t| t.id == "main.js").collect();
  let cases = expand_cases(&discovered, &Filter::All).unwrap();
  assert_eq!(cases.len(), 1);
  assert_eq!(cases[0].variant, Variant::Module);

  let case = &cases[0];
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
  assert_eq!(js.typ.as_deref(), Some("SyntaxError"));
  assert!(!js.message.is_empty(), "expected non-empty message");
  assert!(
    js.message.contains("bad.js"),
    "expected message to mention bad.js, got: {}",
    js.message
  );
}

