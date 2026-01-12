use conformance_harness::{Expectations, TimeoutManager};
use std::fs;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;
use test262_semantic::discover::discover_tests;
use test262_semantic::executor::{ExecResult, Executor};
use test262_semantic::harness::HarnessMode;
use test262_semantic::report::{TestOutcome, Variant};
use test262_semantic::runner::{expand_cases, Filter, TestCase};
 
#[test]
fn raw_module_is_executed_verbatim_without_injected_module_separator() {
  // Sentinel string that should appear at byte 0 of the source seen by the executor.
  const SENTINEL: &str = "// RAW_MODULE_SENTINEL\n";
 
  let temp = tempdir().unwrap();
 
  // Minimal fake test262 checkout: only `test/` is required. Intentionally omit `harness/` to ensure
  // `flags: [raw]` forces effective harness suppression even when the runner is invoked with a
  // harness mode that would normally read the harness directory.
  let test_dir = temp.path().join("test");
  fs::create_dir_all(&test_dir).unwrap();
 
  fs::write(
    test_dir.join("raw_module.js"),
    format!(
      "{SENTINEL}/*---\nflags: [module, raw]\n---*/\nexport const x = 1;\n"
    ),
  )
  .unwrap();
 
  let discovered = discover_tests(temp.path()).unwrap();
  let cases = expand_cases(&discovered, &Filter::All).unwrap();
 
  let expectations = Expectations::empty();
  let timeout_manager = TimeoutManager::new();
 
  struct AssertingExecutor {
    inner: Box<dyn Executor>,
  }
 
  impl Executor for AssertingExecutor {
    fn execute(&self, case: &TestCase, source: &str, cancel: &Arc<AtomicBool>) -> ExecResult {
      if case.id == "raw_module.js" && case.variant == Variant::Module {
        assert!(
          source.as_bytes().starts_with(SENTINEL.as_bytes()),
          "raw module source should be passed to executor verbatim (no injected marker), got: {source:?}"
        );
      }
      self.inner.execute(case, source, cancel)
    }
  }
 
  let executor = AssertingExecutor {
    inner: test262_semantic::executor::default_executor(),
  };
 
  let results = test262_semantic::runner::run_cases(
    temp.path(),
    HarnessMode::Test262,
    &cases,
    &expectations,
    &executor,
    Duration::from_secs(1),
    &timeout_manager,
  );
 
  assert_eq!(results.len(), 1, "expected exactly one expanded case");
  assert_eq!(results[0].outcome, TestOutcome::Passed);
}
