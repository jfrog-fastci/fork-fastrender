#![cfg(feature = "vmjs")]

use conformance_harness::FailOn;
use js_wpt_dom_runner::{run_suite, BackendSelection, SuiteConfig};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Deserialize)]
struct SubsetManifest {
  tests: Vec<String>,
}

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("../..")
    .canonicalize()
    .expect("canonicalize repo root")
}

fn corpus_root() -> PathBuf {
  repo_root()
    .join("tests/wpt_dom")
    .canonicalize()
    .expect("canonicalize corpus root")
}

fn subset_manifest_path() -> PathBuf {
  repo_root().join("tests/wpt_suites/js_html_integration_wpt_subset.toml")
}

fn load_subset_manifest() -> SubsetManifest {
  let path = subset_manifest_path();
  let raw = std::fs::read_to_string(&path).expect("read subset manifest");
  toml::from_str(&raw).expect("parse subset manifest")
}

#[test]
fn js_html_integration_wpt_subset_passes() {
  let corpus_root = corpus_root();
  let subset = load_subset_manifest();
  assert!(
    !subset.tests.is_empty(),
    "subset manifest should list at least one test"
  );

  let filter = subset.tests.join(",");
  let report = run_suite(&SuiteConfig {
    wpt_root: corpus_root.clone(),
    manifest_path: corpus_root.join("expectations.toml"),
    shard: None,
    filter: Some(filter),
    // HTML integration tests run through `api::BrowserTab`, which has higher initialization overhead
    // than the pure JS backend. Keep this conservative to avoid CI flakiness.
    timeout: Duration::from_secs(5),
    long_timeout: Duration::from_secs(15),
    fail_on: FailOn::New,
    backend: BackendSelection::VmJs,
  })
  .expect("run suite");

  // Keep failure output actionable: print any *unexpected* mismatches with their WPT report payload.
  let unexpected: Vec<_> = report
    .results
    .iter()
    .filter(|r| r.mismatched && !r.expected_mismatch && !r.flaky)
    .collect();

  if !unexpected.is_empty() {
    eprintln!("js_html_integration WPT subset unexpected failures:");
    for r in unexpected {
      eprintln!(
        "- {} ({}): outcome={:?} error={:?} skip_reason={:?}",
        r.id,
        format!("https://web-platform.test/{}", r.id),
        r.outcome,
        r.error,
        r.skip_reason
      );
      if let Some(wpt) = &r.wpt_report {
        eprintln!(
          "  wpt_report: file_status={} harness_status={} message={:?} stack={:?}",
          wpt.file_status, wpt.harness_status, wpt.message, wpt.stack
        );
        for st in &wpt.subtests {
          if st.status != "pass" {
            eprintln!(
              "    subtest: status={} name={} message={:?} stack={:?}",
              st.status, st.name, st.message, st.stack
            );
          }
        }
      } else {
        eprintln!("  (no WptReport payload captured)");
      }
    }
    panic!("js_html_integration WPT subset had unexpected mismatches: {report:#?}");
  }

  assert_eq!(report.summary.timed_out, 0);
  assert_eq!(report.summary.errored, 0);
  assert_eq!(report.summary.skipped, 0);
  if let Some(mismatches) = &report.summary.mismatches {
    assert_eq!(
      mismatches.unexpected, 0,
      "expected zero unexpected mismatches: {report:#?}"
    );
    assert_eq!(
      mismatches.flaky, 0,
      "expected zero flaky mismatches: {report:#?}"
    );
  }
}
