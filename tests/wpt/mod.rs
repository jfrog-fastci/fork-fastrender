//! WPT-style harness for FastRender.
//!
//! This is an internal “render and compare” test harness. It is not a full integration
//! with the upstream Web Platform Tests suite and is expected to evolve alongside the
//! renderer internals.
//!
//! For repo-level guidance on how this fits into testing workflows, see `docs/testing.md`.

pub mod harness;
pub mod runner;
#[cfg(test)]
mod validate_manifest;
#[cfg(test)]
mod offline_invariants;

fn init_rayon_for_wpt_tests() {
  crate::common::init_rayon_for_tests(1);
}

pub(crate) fn create_test_renderer() -> fastrender::FastRender {
  crate::common::init_rayon_for_tests(1);
  let config = fastrender::FastRenderConfig::default()
    .with_font_sources(fastrender::FontConfig::bundled_only())
    .with_resource_policy(
      fastrender::ResourcePolicy::default()
        .allow_http(false)
        .allow_https(false),
    )
    .with_paint_parallelism(fastrender::PaintParallelism::disabled())
    .with_layout_parallelism(fastrender::LayoutParallelism::disabled());
  fastrender::FastRender::with_config(config).expect("build renderer")
}

// Re-export main types for convenience
pub use harness::compare_images;
pub use harness::generate_diff_image;
pub use fastrender::image_compare::CompareConfig;
pub use fastrender::image_compare::DiffStatistics;
pub use fastrender::image_compare::ImageDiff;
pub use harness::AssertionResult;
pub use harness::DiscoveryMode;
pub use harness::HarnessConfig;
pub use harness::ReftestExpectation;
pub use harness::SuiteResult;
pub use harness::TestMetadata;
pub use harness::TestResult;
pub use harness::TestStatus;
pub use harness::TestType;
pub use runner::RunnerStats;
pub use runner::WptRunner;
pub use runner::WptRunnerBuilder;

#[test]
fn wpt_local_suite_passes() {
  use std::path::{Path, PathBuf};

  crate::common::with_large_stack(|| {
    let renderer = create_test_renderer();
    let mut config = HarnessConfig::default();
    // The discovery directory under `tests/wpt/tests/` contains harness-focused metadata fixtures
    // (expected failures, disables, etc.). Keep the smoke-test suite focused on the curated
    // manifest entries so UPDATE_WPT_EXPECTED mode doesn't trip over those fixtures.
    config.discovery_mode = DiscoveryMode::ManifestOnly;
    config.expected_dir = std::env::var_os("WPT_EXPECTED_DIR")
      .map(PathBuf::from)
      .unwrap_or_else(|| PathBuf::from("tests/wpt/expected"));
    config.update_expected = std::env::var_os("UPDATE_WPT_EXPECTED").is_some();
    let filter = std::env::var("WPT_FILTER")
      .ok()
      .map(|value| value.trim().to_string())
      .and_then(|value| (!value.is_empty()).then_some(value));
    config.filter = filter.clone();

    let mut runner = WptRunner::with_config(renderer, config);

    let results = runner.run_suite(Path::new("tests/wpt/tests"));
    assert!(
      !results.is_empty(),
      "WPT run produced no results (WPT_FILTER={})",
      filter.as_deref().unwrap_or("<unset>")
    );

    let runnable = results
      .iter()
      .filter(|result| result.status != TestStatus::Skip)
      .count();
    assert!(
      runnable > 0,
      "WPT filter matched no runnable tests (WPT_FILTER={})",
      filter.as_deref().unwrap_or("<unset>")
    );

    for result in &results {
      assert!(
        !result.status.is_failure(),
        "{} failed with status {:?}",
        result.metadata.id,
        result.status
      );
    }
  });
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod offline_invariants;

#[cfg(test)]
mod integration_tests {
  use super::*;
  use std::path::PathBuf;
  use tempfile::TempDir;

  /// Test that we can create a runner and run tests
  #[test]
  fn test_wpt_runner_integration() {
    let renderer = super::create_test_renderer();
    let runner = WptRunner::new(renderer);

    // Runner should start with empty stats
    assert_eq!(runner.stats().total, 0);
    assert_eq!(runner.stats().passed, 0);
    assert_eq!(runner.stats().failed, 0);
  }

  /// Test running a suite on an empty directory
  #[test]
  fn test_empty_suite() {
    let renderer = super::create_test_renderer();
    let mut runner = WptRunner::new(renderer);

    let temp = TempDir::new().unwrap();
    let results = runner.run_suite(temp.path());

    assert!(results.is_empty());
  }

  /// Test suite aggregation
  #[test]
  fn test_suite_aggregation() {
    let renderer = super::create_test_renderer();
    let mut runner = WptRunner::new(renderer);

    let temp = TempDir::new().unwrap();

    // Create test files
    std::fs::write(
      temp.path().join("test1.html"),
      "<!DOCTYPE html><html><body><div>Test 1</div></body></html>",
    )
    .unwrap();
    std::fs::write(
      temp.path().join("test2.html"),
      "<!DOCTYPE html><html><body><div>Test 2</div></body></html>",
    )
    .unwrap();

    let suite = runner.run_suite_aggregated(temp.path());

    assert_eq!(suite.total(), 2);
    assert!(suite.duration.as_nanos() > 0);
  }

  /// Test filtering by pattern
  #[test]
  fn test_filter_pattern() {
    let renderer = super::create_test_renderer();
    let config = HarnessConfig::default().with_filter("box-model");
    let mut runner = WptRunner::with_config(renderer, config);

    let temp = TempDir::new().unwrap();

    // Create test files with different names
    std::fs::write(temp.path().join("box-model-test.html"), "<html></html>").unwrap();
    std::fs::write(temp.path().join("margin-test.html"), "<html></html>").unwrap();

    let results = runner.run_suite(temp.path());

    // One should pass filter (box-model), one should be skipped
    let skipped = results
      .iter()
      .filter(|r| r.status == TestStatus::Skip)
      .count();
    let not_skipped = results
      .iter()
      .filter(|r| r.status != TestStatus::Skip)
      .count();

    // box-model-test.html passes filter, margin-test.html is filtered out
    assert_eq!(skipped, 1);
    assert!(not_skipped >= 1);
  }

  /// Test harness config builder pattern
  #[test]
  fn test_harness_config_builder() {
    let config = HarnessConfig::with_test_dir("custom/path")
      .with_tolerance(10)
      .with_max_diff(0.5)
      .fail_fast()
      .with_filter("css")
      .with_discovery_mode(DiscoveryMode::MetadataOnly)
      .with_font_dir("fonts/ci");

    assert_eq!(config.test_dir, PathBuf::from("custom/path"));
    assert_eq!(config.pixel_tolerance, 10);
    assert_eq!(config.max_diff_percentage, 0.5);
    assert!(config.fail_fast);
    assert_eq!(config.filter, Some("css".to_string()));
    assert_eq!(config.discovery_mode, DiscoveryMode::MetadataOnly);
    assert_eq!(config.font_dirs, vec![PathBuf::from("fonts/ci")]);
  }

  /// Test runner builder pattern
  #[test]
  fn test_runner_builder_pattern() {
    let renderer = super::create_test_renderer();
    let runner = WptRunnerBuilder::new()
      .renderer(renderer)
      .test_dir("tests/custom")
      .expected_dir("tests/expected")
      .output_dir("target/output")
      .tolerance(5)
      .max_diff(1.0)
      .fail_fast()
      .save_rendered()
      .save_diffs()
      .build();

    assert_eq!(runner.config().test_dir, PathBuf::from("tests/custom"));
    assert_eq!(
      runner.config().expected_dir,
      PathBuf::from("tests/expected")
    );
    assert_eq!(runner.config().output_dir, PathBuf::from("target/output"));
    assert_eq!(runner.config().pixel_tolerance, 5);
    assert_eq!(runner.config().max_diff_percentage, 1.0);
    assert!(runner.config().fail_fast);
    assert!(runner.config().save_rendered);
    assert!(runner.config().save_diffs);
  }

  /// Test statistics tracking
  #[test]
  fn test_stats_tracking() {
    let renderer = super::create_test_renderer();
    let mut runner = WptRunner::new(renderer);

    let temp = TempDir::new().unwrap();

    // Create a crashtest (doesn't need reference file)
    std::fs::write(
      temp.path().join("crashtest.html"),
      "<!DOCTYPE html><html><body>Crash test</body></html>",
    )
    .unwrap();

    let _result = runner.run_test(&temp.path().join("crashtest.html"));

    assert_eq!(runner.stats().total, 1);

    // Reset stats
    runner.reset_stats();
    assert_eq!(runner.stats().total, 0);
  }

  /// Test TestResult creation helpers
  #[test]
  fn test_result_creation() {
    let metadata = TestMetadata::from_path(PathBuf::from("test.html"));

    let pass = TestResult::pass(metadata.clone(), std::time::Duration::from_millis(100));
    assert_eq!(pass.status, TestStatus::Pass);

    let fail = TestResult::fail(
      metadata.clone(),
      std::time::Duration::from_millis(50),
      "Image mismatch",
    );
    assert_eq!(fail.status, TestStatus::Fail);
    assert!(fail.message.is_some());

    let error = TestResult::error(
      metadata.clone(),
      std::time::Duration::from_millis(10),
      "Parse error",
    );
    assert_eq!(error.status, TestStatus::Error);

    let skip = TestResult::skip(metadata.clone(), "Not supported");
    assert_eq!(skip.status, TestStatus::Skip);
    assert_eq!(skip.duration, std::time::Duration::ZERO);

    let timeout = TestResult::timeout(metadata.clone(), std::time::Duration::from_secs(30));
    assert_eq!(timeout.status, TestStatus::Timeout);
  }

  /// Test metadata from path extraction
  #[test]
  fn test_metadata_from_path() {
    let path = PathBuf::from("/path/to/tests/wpt/css/box-model-001.html");
    let metadata = TestMetadata::from_path(path.clone());

    assert_eq!(metadata.id, "box-model-001");
    assert_eq!(metadata.path, path);
    assert_eq!(metadata.test_type, TestType::Reftest);
    assert!(!metadata.disabled);
  }

  /// Test suite result display
  #[test]
  fn test_suite_result_display() {
    let mut suite = SuiteResult::new("test-suite");

    suite.add_result(TestResult::pass(
      TestMetadata::from_path(PathBuf::from("test1.html")),
      std::time::Duration::from_millis(100),
    ));
    suite.add_result(TestResult::fail(
      TestMetadata::from_path(PathBuf::from("test2.html")),
      std::time::Duration::from_millis(50),
      "Failed",
    ));
    suite.finalize();

    let display = format!("{}", suite);

    assert!(display.contains("Suite: test-suite"));
    assert!(display.contains("Total: 2"));
    assert!(display.contains("Passed: 1"));
    assert!(display.contains("Failed: 1"));
    assert!(display.contains("Pass Rate: 50.0%"));
  }
}
