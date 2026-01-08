use js_wpt_dom_runner::{discover_tests, RunOutcome, Runner, RunnerConfig};
use js_wpt_dom_runner::wpt_fs::WptFs;
use std::path::PathBuf;

fn fixtures_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("../../tests/wpt_dom/tests")
    .canonicalize()
    .expect("canonicalize fixtures root")
}

#[test]
fn runs_window_js_smoke_test() {
  let root = fixtures_root();
  let tests = discover_tests(&root).expect("discover tests");
  let test = tests
    .iter()
    .find(|t| t.id == "smoke/meta_script.window.js")
    .expect("missing meta_script.window.js");

  let fs = WptFs::new(&root).expect("wpt fs");
  let runner = Runner::new(fs, RunnerConfig::default());
  let result = runner.run_test(test).expect("run test");
  assert_eq!(result.outcome, RunOutcome::Pass);
}

#[test]
fn discovers_worker_tests_but_skips_them() {
  let root = fixtures_root();
  let tests = discover_tests(&root).expect("discover tests");
  let test = tests
    .iter()
    .find(|t| t.id == "smoke/unsupported.worker.js")
    .expect("missing unsupported.worker.js");

  let fs = WptFs::new(&root).expect("wpt fs");
  let runner = Runner::new(fs, RunnerConfig::default());
  let result = runner.run_test(test).expect("run test");
  match result.outcome {
    RunOutcome::Skip(reason) => {
      assert!(reason.contains("worker"), "reason should mention worker");
    }
    other => panic!("expected Skip, got {other:?}"),
  }
}

