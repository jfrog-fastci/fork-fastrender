use js_wpt_dom_runner::{discover_tests, RunOutcome, Runner, RunnerConfig, WptFs};
use std::path::PathBuf;

fn corpus_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("../../tests/wpt_dom")
    .canonicalize()
    .expect("canonicalize corpus root")
}

fn tests_root() -> PathBuf {
  corpus_root().join("tests")
}

#[test]
fn runs_window_js_smoke_test() {
  let corpus_root = corpus_root();
  let tests_root = tests_root();
  let tests = discover_tests(&tests_root).expect("discover tests");
  let test = tests
    .iter()
    .find(|t| t.id == "smoke/meta_script.window.js")
    .expect("missing meta_script.window.js");

  let fs = WptFs::new(&corpus_root).expect("wpt fs");
  let runner = Runner::new(fs, RunnerConfig::default());
  let result = runner.run_test(test).expect("run test");
  assert_eq!(result.outcome, RunOutcome::Pass);
}

#[test]
fn discovers_worker_tests_but_skips_them() {
  let corpus_root = corpus_root();
  let tests_root = tests_root();
  let tests = discover_tests(&tests_root).expect("discover tests");
  let test = tests
    .iter()
    .find(|t| t.id == "smoke/unsupported.worker.js")
    .expect("missing unsupported.worker.js");

  let fs = WptFs::new(&corpus_root).expect("wpt fs");
  let runner = Runner::new(fs, RunnerConfig::default());
  let result = runner.run_test(test).expect("run test");
  match result.outcome {
    RunOutcome::Skip(reason) => {
      assert!(reason.contains("worker"), "reason should mention worker");
    }
    other => panic!("expected Skip, got {other:?}"),
  }
}
