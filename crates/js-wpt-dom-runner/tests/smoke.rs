use js_wpt_dom_runner::{
  discover_tests, BackendKind, BackendSelection, RunOutcome, Runner, RunnerConfig, WptFs,
};
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

#[test]
fn cargo_toml_does_not_depend_on_selectors() {
  // `selectors` is a heavy-ish dependency used elsewhere in the workspace, but `js-wpt-dom-runner`
  // should not need it. Keep a lightweight regression guard here so the edge doesn't get
  // reintroduced by accident.
  let cargo_toml = fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
    .expect("read js-wpt-dom-runner/Cargo.toml");
  assert!(
    !cargo_toml.contains("dep:selectors"),
    "js-wpt-dom-runner vmjs feature should not include dep:selectors"
  );
  assert!(
    !cargo_toml
      .lines()
      .any(|line| matches!(line.trim_start(), l if l.starts_with("selectors ") || l.starts_with("selectors="))),
    "js-wpt-dom-runner should not declare a direct selectors dependency"
  );
}

fn corpus_root() -> PathBuf {
  static ROOT: OnceLock<PathBuf> = OnceLock::new();
  ROOT
    .get_or_init(|| {
      PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/wpt_dom")
        .canonicalize()
        .expect("canonicalize corpus root")
    })
    .clone()
}

fn tests_root() -> PathBuf {
  static ROOT: OnceLock<PathBuf> = OnceLock::new();
  ROOT.get_or_init(|| corpus_root().join("tests")).clone()
}

fn wpt_fs() -> WptFs {
  static FS: OnceLock<WptFs> = OnceLock::new();
  FS.get_or_init(|| WptFs::new(corpus_root()).expect("wpt fs"))
    .clone()
}

fn discovered_tests() -> &'static Vec<js_wpt_dom_runner::TestCase> {
  static TESTS: OnceLock<Vec<js_wpt_dom_runner::TestCase>> = OnceLock::new();
  TESTS.get_or_init(|| discover_tests(&tests_root()).expect("discover tests"))
}

fn smoke_config() -> RunnerConfig {
  // Keep tests fast + deterministic: the WPT corpus here is tiny and should not need the runner's
  // generous 5s/30s defaults.
  RunnerConfig {
    default_timeout: Duration::from_millis(500),
    long_timeout: Duration::from_secs(2),
    ..RunnerConfig::default()
  }
}

fn run_test_id_all_backends(
  id: &str,
  config: RunnerConfig,
) -> Vec<(BackendKind, js_wpt_dom_runner::RunResult)> {
  let tests = discovered_tests();
  let test = tests
    .iter()
    .find(|t| t.id == id)
    .unwrap_or_else(|| panic!("missing test {id}"));

  let mut out = Vec::new();
  for backend in BackendKind::all_available() {
    let mut config = config.clone();
    config.backend = match backend {
      BackendKind::QuickJs => BackendSelection::QuickJs,
      BackendKind::VmJs => BackendSelection::VmJs,
      BackendKind::VmJsRendered => BackendSelection::VmJsRendered,
    };

    let runner = Runner::new(wpt_fs(), config);
    let result = runner.run_test(test).expect("run test");
    out.push((backend, result));
  }
  out
}

fn run_test_id_backend(
  id: &str,
  backend: BackendSelection,
  config: RunnerConfig,
) -> js_wpt_dom_runner::RunResult {
  let tests = discovered_tests();
  let test = tests
    .iter()
    .find(|t| t.id == id)
    .unwrap_or_else(|| panic!("missing test {id}"));

  let mut config = config.clone();
  config.backend = backend;

  let runner = Runner::new(wpt_fs(), config);
  runner.run_test(test).expect("run test")
}

fn assert_wpt_pass(id: &str) {
  for (backend, result) in run_test_id_all_backends(id, smoke_config()) {
    assert_eq!(
      result.outcome,
      RunOutcome::Pass,
      "{id} should pass under backend {backend}"
    );
    let report = result
      .wpt_report
      .unwrap_or_else(|| panic!("{id} should include report payload under backend {backend}"));
    assert_eq!(report.file_status, "pass");
    assert_eq!(
      report.harness_status, "ok",
      "{id} should have harness_status=ok under backend {backend}: {report:#?}"
    );

    assert!(
      !report.subtests.is_empty(),
      "{id} should include at least one subtest under backend {backend}: {report:#?}"
    );

    for st in &report.subtests {
      assert!(
        !st.name.is_empty(),
        "{id} subtest name should be non-empty under backend {backend}: {st:#?}"
      );
      assert!(
        matches!(st.status.as_str(), "pass" | "fail" | "timeout" | "error"),
        "{id} subtest status should be one of pass|fail|timeout|error under backend {backend}: {st:#?}"
      );
    }
  }
}

#[test]
fn reports_subtest_failures() {
  for (backend, result) in run_test_id_all_backends("smoke/sync-fail.html", smoke_config()) {
    match &result.outcome {
      RunOutcome::Fail(_msg) => {}
      other => panic!("sync-fail.html should fail under backend {backend}, got {other:?}"),
    }

    let report = result.wpt_report.unwrap_or_else(|| {
      panic!("sync-fail.html should include report payload under backend {backend}")
    });
    assert_eq!(
      report.file_status, "fail",
      "sync-fail.html should report file_status=fail under backend {backend}: {report:#?}"
    );
    assert!(
      report.subtests.iter().any(|st| st.status == "fail"),
      "sync-fail.html should include at least one failing subtest under backend {backend}: {report:#?}"
    );
  }
}

#[test]
fn runs_window_js_smoke_test() {
  assert_wpt_pass("smoke/meta_script.window.js");
}

#[test]
fn runs_any_js_in_window_realm() {
  assert_wpt_pass("smoke/any_promise.any.js");
}

#[test]
fn window_or_worker_global_scope_primitives() {
  assert_wpt_pass("smoke/window_or_worker_global_scope.window.js");
}

#[test]
fn runs_html_sync_test() {
  assert_wpt_pass("smoke/sync-pass.html");
}

#[test]
fn runs_html_async_test() {
  assert_wpt_pass("smoke/async-timeout-pass.html");
}

#[test]
fn runs_html_promise_test() {
  assert_wpt_pass("smoke/promise-pass.html");
}

#[test]
fn runs_setinterval_and_clearinterval() {
  assert_wpt_pass("smoke/interval_cancel.window.js");
}

#[test]
#[cfg(feature = "vmjs")]
fn range_constructor_is_exposed_in_vmjs_backend() {
  let result = run_test_id_backend(
    "smoke/range_constructor.window.js",
    BackendSelection::VmJs,
    smoke_config(),
  );
  assert_eq!(result.outcome, RunOutcome::Pass);
  let report = result
    .wpt_report
    .expect("expected WPT report payload for range_constructor.window.js");
  assert_eq!(report.file_status, "pass");
  assert_eq!(report.harness_status, "ok");
}

#[test]
fn runs_node_contains_smoke_test() {
  assert_wpt_pass("smoke/node_contains.window.js");
}

#[test]
fn runs_node_remove_smoke_test() {
  assert_wpt_pass("smoke/node_remove.window.js");
}

#[test]
fn runs_node_has_child_nodes_smoke_test() {
  assert_wpt_pass("smoke/node_has_child_nodes.window.js");
}

#[test]
fn runs_node_insert_before_smoke_test() {
  assert_wpt_pass("smoke/node_insert_before.window.js");
}

#[test]
fn runs_node_replace_child_smoke_test() {
  assert_wpt_pass("smoke/node_replace_child.window.js");
}

#[test]
fn runs_fetch_relative_url_smoke_test() {
  assert_wpt_pass("smoke/fetch_relative.window.js");
}

#[test]
fn runs_dom_shims_window_js() {
  assert_wpt_pass("smoke/dom_shims.window.js");
}

#[test]
fn runs_step_func_smoke_test() {
  assert_wpt_pass("smoke/step_func.window.js");
}

#[test]
fn runs_document_head_body_smoke_test() {
  assert_wpt_pass("smoke/document_head_body.window.js");
}

#[test]
fn meta_timeout_long_overrides_runner_default_timeout() {
  for (backend, result) in run_test_id_all_backends(
    "smoke/timeout_long.window.js",
    RunnerConfig {
      default_timeout: Duration::from_millis(10),
      long_timeout: Duration::from_millis(250),
      ..RunnerConfig::default()
    },
  ) {
    assert_eq!(
      result.outcome,
      RunOutcome::Pass,
      "timeout_long should pass under backend {backend}"
    );
    let report = result.wpt_report.unwrap_or_else(|| {
      panic!("timeout_long should include report payload under backend {backend}")
    });
    assert_eq!(report.file_status, "pass");
    assert!(
      !report.subtests.is_empty(),
      "timeout_long should include at least one subtest under backend {backend}: {report:#?}"
    );
  }
}

#[test]
fn discovers_worker_tests_but_skips_them() {
  for (backend, result) in run_test_id_all_backends("smoke/unsupported.worker.js", smoke_config()) {
    match result.outcome {
      RunOutcome::Skip(reason) => {
        assert!(
          reason.contains("worker"),
          "reason should mention worker under backend {backend}"
        );
      }
      other => panic!("expected Skip under backend {backend}, got {other:?}"),
    }
  }
}

#[test]
fn discovers_serviceworker_tests_but_skips_them() {
  for (backend, result) in
    run_test_id_all_backends("smoke/unsupported.serviceworker.js", smoke_config())
  {
    match result.outcome {
      RunOutcome::Skip(reason) => {
        assert!(
          reason.contains("service worker"),
          "reason should mention service worker under backend {backend}: {reason}"
        );
      }
      other => panic!("expected Skip under backend {backend}, got {other:?}"),
    }
  }
}

#[test]
fn discovers_sharedworker_tests_but_skips_them() {
  for (backend, result) in
    run_test_id_all_backends("smoke/unsupported.sharedworker.js", smoke_config())
  {
    match result.outcome {
      RunOutcome::Skip(reason) => {
        assert!(
          reason.contains("shared worker"),
          "reason should mention shared worker under backend {backend}: {reason}"
        );
      }
      other => panic!("expected Skip under backend {backend}, got {other:?}"),
    }
  }
}

#[test]
fn runs_domparsing_innerhtml_outerhtml_test() {
  assert_wpt_pass("domparsing/innerhtml-outerhtml.window.js");
}

#[test]
fn runs_domparsing_documentfragment_test() {
  assert_wpt_pass("domparsing/documentfragment.window.js");
}

#[test]
fn runs_domparsing_outerhtml_fragment_test() {
  assert_wpt_pass("domparsing/outerhtml-fragment.window.js");
}

#[test]
fn runs_element_matches_and_closest_test() {
  assert_wpt_pass("dom/element_matches_closest.window.js");
}

#[test]
fn runs_element_query_selector_test() {
  assert_wpt_pass("dom/element_query_selector.window.js");
}

#[test]
fn runs_document_fragment_append_semantics_test() {
  assert_wpt_pass("dom/document_fragment_append.window.js");
}

#[test]
fn runs_node_sibling_props_test() {
  assert_wpt_pass("dom/node_sibling_props.window.js");
}

#[test]
fn runs_error_constructor_smoke_test() {
  assert_wpt_pass("dom/error_constructor.window.js");
}

#[test]
fn runs_eventtarget_smoke_tests() {
  assert_wpt_pass("events/eventtarget.window.js");
  assert_wpt_pass("events/eventtarget_dispatch_order.window.js");
  assert_wpt_pass("events/eventtarget_order.window.js");
  assert_wpt_pass("events/document_eventtarget_path.window.js");
  assert_wpt_pass("events/passive_listener.window.js");
}

#[test]
fn runs_event_loop_tests() {
  assert_wpt_pass("event_loop/promise_then_before_timeout.window.js");
  assert_wpt_pass("event_loop/queue_microtask_before_timeout.window.js");
  assert_wpt_pass("event_loop/queue_microtask_order.window.js");
  assert_wpt_pass("event_loop/settimeout_args.window.js");
  assert_wpt_pass("event_loop/setinterval_cancel.window.js");
}

#[test]
fn runs_get_element_by_id_test() {
  assert_wpt_pass("dom/document_get_element_by_id.window.js");
}

#[test]
#[cfg(feature = "vmjs")]
fn runs_urlsearchparams_live_test_vmjs() {
  let result = run_test_id_backend(
    "url/urlsearchparams-live.window.js",
    BackendSelection::VmJs,
    smoke_config(),
  );
  assert_eq!(
    result.outcome,
    RunOutcome::Pass,
    "urlsearchparams-live should pass under vmjs"
  );
  let report = result
    .wpt_report
    .expect("urlsearchparams-live should include report payload under vmjs");
  assert_eq!(report.file_status, "pass");
  assert_eq!(report.harness_status, "ok");
  assert!(
    !report.subtests.is_empty(),
    "urlsearchparams-live should include subtests under vmjs: {report:#?}"
  );
  for st in &report.subtests {
    assert!(!st.name.is_empty());
    assert!(matches!(
      st.status.as_str(),
      "pass" | "fail" | "timeout" | "error"
    ));
  }
}

#[test]
fn classifies_vmjs_termination_as_timeout() {
  if !BackendKind::VmJs.is_available() {
    return;
  }

  let result = run_test_id_backend(
    "smoke/infinite_loop_timeout.window.js",
    BackendSelection::VmJs,
    RunnerConfig {
      // Shorten the default timeout; this test intentionally loops forever and should terminate
      // quickly once the deadline is reached.
      default_timeout: Duration::from_millis(250),
      long_timeout: Duration::from_secs(1),
      ..RunnerConfig::default()
    },
  );
  assert_eq!(
    result.outcome,
    RunOutcome::Timeout,
    "vm-js backend should classify engine termination as a timeout"
  );
}
