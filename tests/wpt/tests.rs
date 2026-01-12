use super::*;

mod wpt_runner_tests {
  use super::AssertionResult;
  use super::DiscoveryMode;
  use super::HarnessConfig;
  use super::CompareConfig;
  use super::RunnerStats;
  use super::SuiteResult;
  use super::TestMetadata;
  use super::TestResult;
  use super::TestStatus;
  use super::TestType;
  use super::WptRunner;
  use super::WptRunnerBuilder;
  use image::codecs::png::PngEncoder;
  use image::{ColorType, ImageEncoder, Rgba, RgbaImage};
  use serde_json::Value as JsonValue;
  use std::collections::HashMap;
  use std::path::Path;
  use std::path::PathBuf;
  use std::time::Duration;
  use tempfile::TempDir;
  use walkdir::WalkDir;

  // =========================================================================
  // WptRunner Tests
  // =========================================================================

  #[test]
  fn test_wpt_runner_new() {
    let renderer = super::create_test_renderer();
    let runner = WptRunner::new(renderer);

    assert_eq!(runner.stats().total, 0);
    assert_eq!(runner.config().pixel_tolerance, 0);
  }

  #[test]
  fn test_wpt_runner_with_config() {
    let renderer = super::create_test_renderer();
    let config = HarnessConfig::default()
      .with_tolerance(10)
      .with_max_diff(0.5);
    let runner = WptRunner::with_config(renderer, config);

    assert_eq!(runner.config().pixel_tolerance, 10);
    assert_eq!(runner.config().max_diff_percentage, 0.5);
  }

  #[test]
  fn test_wpt_runner_builder() {
    let renderer = super::create_test_renderer();
    let runner = WptRunnerBuilder::new()
      .renderer(renderer)
      .test_dir("custom/tests")
      .expected_dir("custom/expected")
      .output_dir("target/custom-output")
      .tolerance(5)
      .max_diff(1.0)
      .fail_fast()
      .save_rendered()
      .save_diffs()
      .parallel(2)
      .manifest("custom/manifest.toml")
      .discovery_mode(DiscoveryMode::ManifestOnly)
      .font_dir("fonts/ci-builder")
      .no_report()
      .build();

    assert_eq!(runner.config().test_dir, PathBuf::from("custom/tests"));
    assert_eq!(
      runner.config().expected_dir,
      PathBuf::from("custom/expected")
    );
    assert_eq!(
      runner.config().output_dir,
      PathBuf::from("target/custom-output")
    );
    assert_eq!(runner.config().pixel_tolerance, 5);
    assert_eq!(runner.config().max_diff_percentage, 1.0);
    assert!(runner.config().fail_fast);
    assert!(runner.config().save_rendered);
    assert!(runner.config().save_diffs);
    assert!(runner.config().parallel);
    assert_eq!(runner.config().workers, 2);
    assert_eq!(
      runner.config().manifest_path,
      Some(PathBuf::from("custom/manifest.toml"))
    );
    assert!(!runner.config().write_report);
    assert_eq!(runner.config().discovery_mode, DiscoveryMode::ManifestOnly);
    assert_eq!(
      runner.config().font_dirs,
      vec![PathBuf::from("fonts/ci-builder")]
    );
  }

  #[test]
  fn test_wpt_runner_stats_tracking() {
    let mut stats = RunnerStats::default();

    assert_eq!(stats.total, 0);
    assert_eq!(stats.passed, 0);
    assert_eq!(stats.failed, 0);
    assert_eq!(stats.pass_rate(), 100.0);

    stats.total = 10;
    stats.passed = 7;
    stats.failed = 2;
    stats.errors = 1;

    assert_eq!(stats.pass_rate(), 70.0);
  }

  #[test]
  fn test_wpt_runner_stats_reset() {
    let renderer = super::create_test_renderer();
    let mut runner = WptRunner::new(renderer);

    // Run some tests to populate stats
    let temp = TempDir::new().unwrap();
    std::fs::write(
      temp.path().join("crashtest.html"),
      "<!DOCTYPE html><html><body>Test</body></html>",
    )
    .unwrap();

    runner.run_test(&temp.path().join("crashtest.html"));
    assert!(runner.stats().total > 0);

    runner.reset_stats();
    assert_eq!(runner.stats().total, 0);
  }

  #[test]
  fn test_wpt_runner_empty_suite() {
    let renderer = super::create_test_renderer();
    let mut runner = WptRunner::new(renderer);

    let temp = TempDir::new().unwrap();
    let results = runner.run_suite(temp.path());

    assert!(results.is_empty());
  }

  #[test]
  fn test_wpt_runner_suite_with_tests() {
    let renderer = super::create_test_renderer();
    let mut runner = WptRunner::new(renderer);

    let temp = TempDir::new().unwrap();

    // Create crash tests (don't need reference files)
    std::fs::write(
      temp.path().join("crashtest1.html"),
      "<!DOCTYPE html><html><body><div>Test 1</div></body></html>",
    )
    .unwrap();
    std::fs::write(
      temp.path().join("crashtest2.html"),
      "<!DOCTYPE html><html><body><div>Test 2</div></body></html>",
    )
    .unwrap();

    let results = runner.run_suite(temp.path());

    assert_eq!(results.len(), 2);
  }

  #[test]
  fn wpt_writes_json_report() {
    let renderer = super::create_test_renderer();
    let temp = TempDir::new().unwrap();
    let suite_dir = temp.path().join("suite");
    std::fs::create_dir_all(&suite_dir).unwrap();

    std::fs::write(
      suite_dir.join("sample.html"),
      r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; width: 100%; height: 100%; background: rgb(0, 255, 0); }
    </style>
  </head>
  <body></body>
</html>"#,
    )
    .unwrap();
    std::fs::write(suite_dir.join("sample.ini"), "viewport: 10x10\n").unwrap();

    let expected_dir = suite_dir.join("expected");
    write_solid_png(&expected_dir.join("sample.png"), 10, 10, [255, 0, 0, 255]);

    std::fs::write(
      expected_dir.join("README.txt"),
      "Expected images live in this directory.",
    )
    .unwrap();

    let out_dir = temp.path().join("out");
    let mut config = HarnessConfig::default();
    config.test_dir = suite_dir.clone();
    config.expected_dir = expected_dir;
    config.output_dir = out_dir.clone();
    config.discovery_mode = DiscoveryMode::MetadataOnly;

    let mut runner = WptRunner::with_config(renderer, config);
    let results = runner.run_suite(&suite_dir);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, TestStatus::Fail);

    let report_json_path = out_dir.join("report.json");
    assert!(report_json_path.exists());

    let data = std::fs::read(&report_json_path).unwrap();
    let report: JsonValue = serde_json::from_slice(&data).unwrap();

    assert_eq!(report["schema_version"].as_u64(), Some(1));
    assert!(report["suite"]["timestamp"].as_str().is_some());
    assert!(report["suite"]["duration_ms"].as_u64().is_some());

    assert_eq!(report["summary"]["total"].as_u64(), Some(1));
    assert_eq!(report["summary"]["fail"].as_u64(), Some(1));

    let tests = report["tests"].as_array().unwrap();
    assert_eq!(tests.len(), 1);
    let test = &tests[0];

    assert_eq!(test["id"].as_str(), Some("sample"));
    assert_eq!(test["path"].as_str(), Some("sample.html"));
    assert_eq!(test["test_type"].as_str(), Some("visual"));
    assert!(test["reference"].is_null());
    assert_eq!(test["status"].as_str(), Some("FAIL"));

    assert_eq!(
      test["artifacts"]["expected"].as_str(),
      Some("sample/expected.png")
    );
    assert_eq!(
      test["artifacts"]["actual"].as_str(),
      Some("sample/actual.png")
    );
    assert_eq!(test["artifacts"]["diff"].as_str(), Some("sample/diff.png"));

    assert!(out_dir.join("sample").join("expected.png").exists());
    assert!(out_dir.join("sample").join("actual.png").exists());
    assert!(out_dir.join("sample").join("diff.png").exists());
  }

  #[test]
  fn test_wpt_runner_filter() {
    let renderer = super::create_test_renderer();
    let config = HarnessConfig::default().with_filter("box-model");
    let mut runner = WptRunner::with_config(renderer, config);

    let temp = TempDir::new().unwrap();

    // Create tests with different names
    std::fs::write(temp.path().join("box-model-test.html"), "<html></html>").unwrap();
    std::fs::write(temp.path().join("margin-test.html"), "<html></html>").unwrap();

    let results = runner.run_suite(temp.path());

    // margin-test should be filtered out (skipped)
    let skipped = results
      .iter()
      .filter(|r| r.status == TestStatus::Skip)
      .count();
    assert!(skipped >= 1);
  }

  #[test]
  fn test_wpt_runner_sidecar_discovery() {
    let renderer = super::create_test_renderer();
    let mut config = HarnessConfig::with_test_dir("tests/wpt/tests/discovery")
      .with_discovery_mode(DiscoveryMode::MetadataOnly);
    let temp = TempDir::new().unwrap();
    config.output_dir = temp.path().join("output");
    let mut runner = WptRunner::with_config(renderer, config);

    let results = runner.run_suite(Path::new("tests/wpt/tests/discovery"));
    let map: HashMap<_, _> = results
      .into_iter()
      .map(|r| (r.metadata.id.clone(), r.status))
      .collect();

    assert_eq!(map.get("link-match"), Some(&TestStatus::Pass));
    assert_eq!(map.get("link-mismatch"), Some(&TestStatus::Pass));
    assert_eq!(map.get("ini-expected-fail"), Some(&TestStatus::Pass));
    assert_eq!(map.get("ini-disabled"), Some(&TestStatus::Skip));
  }

  #[test]
  fn wpt_relative_stylesheet_loads_with_base_url() {
    let temp = TempDir::new().unwrap();
    let support_dir = temp.path().join("support");
    std::fs::create_dir_all(&support_dir).unwrap();

    std::fs::write(
      support_dir.join("style.css"),
      r#"body { margin: 0; }
.box { width: 100px; height: 100px; background: rgb(0, 255, 0); }"#,
    )
    .unwrap();

    std::fs::write(
      temp.path().join("test.html"),
      r#"<!doctype html>
<html>
  <head>
    <link rel="match" href="ref.html">
    <link rel="stylesheet" href="support/style.css">
  </head>
  <body><div class="box"></div></body>
</html>"#,
    )
    .unwrap();

    // Inline styles in the reference ensure this only passes if the linked stylesheet in the test
    // document is resolved relative to the test file (base_url set correctly).
    std::fs::write(
      temp.path().join("ref.html"),
      r#"<!doctype html>
<html>
  <head>
    <style>
      body { margin: 0; }
      .box { width: 100px; height: 100px; background: rgb(0, 255, 0); }
    </style>
  </head>
  <body><div class="box"></div></body>
</html>"#,
    )
    .unwrap();

    let mut runner = WptRunnerBuilder::new()
      .test_dir(temp.path())
      .expected_dir(temp.path().join("expected"))
      .output_dir(temp.path().join("out"))
      .no_report()
      .build();

    let result = runner.run_test(&temp.path().join("test.html"));
    assert_eq!(
      result.status,
      TestStatus::Pass,
      "expected PASS, got {:?}: {:?}",
      result.status,
      result.message
    );
  }

  #[test]
  fn wpt_reftest_base_url_isolated_per_document() {
    let temp = TempDir::new().unwrap();

    let test_dir = temp.path().join("test");
    let ref_dir = temp.path().join("ref");
    std::fs::create_dir_all(&test_dir).unwrap();
    std::fs::create_dir_all(ref_dir.join("support")).unwrap();

    // Test uses inline styles.
    std::fs::write(
      test_dir.join("test.html"),
      r#"<!doctype html>
<html>
  <head>
    <link rel="match" href="../ref/ref.html">
    <style>
      body { margin: 0; }
      .box { width: 100px; height: 100px; background: rgb(0, 255, 0); }
    </style>
  </head>
  <body><div class="box"></div></body>
</html>"#,
    )
    .unwrap();

    // Reference loads its stylesheet relative to *its own* directory.
    std::fs::write(
      ref_dir.join("support/style.css"),
      r#"body { margin: 0; }
.box { width: 100px; height: 100px; background: rgb(0, 255, 0); }"#,
    )
    .unwrap();
    std::fs::write(
      ref_dir.join("ref.html"),
      r#"<!doctype html>
<html>
  <head>
    <link rel="stylesheet" href="support/style.css">
  </head>
  <body><div class="box"></div></body>
</html>"#,
    )
    .unwrap();

    let mut runner = WptRunnerBuilder::new()
      .test_dir(temp.path())
      .expected_dir(temp.path().join("expected"))
      .output_dir(temp.path().join("out"))
      .no_report()
      .build();

    let result = runner.run_test(&test_dir.join("test.html"));
    assert_eq!(
      result.status,
      TestStatus::Pass,
      "expected PASS, got {:?}: {:?}",
      result.status,
      result.message
    );
  }

  #[test]
  fn wpt_runner_default_is_offline() {
    let temp = TempDir::new().unwrap();

    let css = r#"body { margin: 0; }
.box { width: 100px; height: 100px; background: rgb(0, 255, 0); }"#;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let saw_request = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saw_request_thread = std::sync::Arc::clone(&saw_request);

    let server = std::thread::spawn(move || {
      use std::io::{Read, Write};
      use std::sync::atomic::Ordering;
      use std::time::{Duration, Instant};

      listener.set_nonblocking(true).unwrap();
      let start = Instant::now();
      while start.elapsed() < Duration::from_millis(500) {
        match listener.accept() {
          Ok((mut stream, _)) => {
            saw_request_thread.store(true, Ordering::SeqCst);
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: text/css\r\nContent-Length: {}\r\n\r\n{}",
              css.len(),
              css
            );
            let _ = stream.write_all(response.as_bytes());
            return;
          }
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            std::thread::sleep(Duration::from_millis(10));
          }
          Err(_) => return,
        }
      }
    });

    let css_url = format!("http://{addr}/style.css");

    std::fs::write(
      temp.path().join("test.html"),
      format!(
        r#"<!doctype html>
<html>
  <head>
    <link rel="match" href="ref.html">
    <link rel="stylesheet" href="{css_url}">
  </head>
  <body><div class="box"></div></body>
</html>"#
      ),
    )
    .unwrap();
    std::fs::write(
      temp.path().join("ref.html"),
      r#"<!doctype html>
<html>
  <head>
    <style>
      body { margin: 0; }
      .box { width: 100px; height: 100px; background: rgb(0, 255, 0); }
    </style>
  </head>
  <body><div class="box"></div></body>
</html>"#,
    )
    .unwrap();

    // Build without supplying a renderer. This should default to an offline renderer with
    // http/https disabled.
    let mut runner = WptRunnerBuilder::new()
      .test_dir(temp.path())
      .expected_dir(temp.path().join("expected"))
      .output_dir(temp.path().join("out"))
      .no_report()
      .build();

    let result = runner.run_test(&temp.path().join("test.html"));
    assert!(
      result.status.is_failure(),
      "expected failure, got {result:?}"
    );

    server.join().unwrap();
    assert!(
      !saw_request.load(std::sync::atomic::Ordering::SeqCst),
      "offline policy should block HTTP requests"
    );
  }

  #[test]
  fn test_wpt_runner_suite_aggregated() {
    let renderer = super::create_test_renderer();
    let mut runner = WptRunner::new(renderer);

    let temp = TempDir::new().unwrap();

    std::fs::write(
      temp.path().join("crashtest1.html"),
      "<!DOCTYPE html><html><body>Test</body></html>",
    )
    .unwrap();
    std::fs::write(
      temp.path().join("crashtest2.html"),
      "<!DOCTYPE html><html><body>Test</body></html>",
    )
    .unwrap();

    let suite = runner.run_suite_aggregated(temp.path());

    assert_eq!(suite.total(), 2);
    assert!(suite.duration.as_nanos() > 0);
  }

  // =========================================================================
  // TestMetadata Tests
  // =========================================================================

  #[test]
  fn test_metadata_from_path() {
    let path = PathBuf::from("/path/to/test-001.html");
    let metadata = TestMetadata::from_path(path.clone());

    assert_eq!(metadata.id, "test-001");
    assert_eq!(metadata.path, path);
    assert_eq!(metadata.viewport_width, 800);
    assert_eq!(metadata.viewport_height, 600);
    assert_eq!(metadata.timeout_ms, 30000);
    assert!(!metadata.disabled);
  }

  #[test]
  fn test_metadata_with_viewport() {
    let metadata = TestMetadata::from_path(PathBuf::from("test.html")).with_viewport(1024, 768);

    assert_eq!(metadata.viewport_width, 1024);
    assert_eq!(metadata.viewport_height, 768);
  }

  #[test]
  fn test_metadata_with_timeout() {
    let metadata = TestMetadata::from_path(PathBuf::from("test.html")).with_timeout(5000);

    assert_eq!(metadata.timeout_ms, 5000);
  }

  #[test]
  fn test_metadata_disabled() {
    let metadata = TestMetadata::from_path(PathBuf::from("test.html")).disable("Not implemented");

    assert!(metadata.disabled);
    assert_eq!(
      metadata.disabled_reason,
      Some("Not implemented".to_string())
    );
  }

  #[test]
  fn test_metadata_expect_status() {
    let metadata = TestMetadata::from_path(PathBuf::from("test.html")).expect(TestStatus::Fail);

    assert_eq!(metadata.expected_status, Some(TestStatus::Fail));
  }

  // =========================================================================
  // TestType Tests
  // =========================================================================

  #[test]
  fn test_type_from_path_reftest() {
    use std::path::Path;

    assert_eq!(
      TestType::from_path(Path::new("test-ref.html")),
      TestType::Reftest
    );
    assert_eq!(
      TestType::from_path(Path::new("test-ref.htm")),
      TestType::Reftest
    );
  }

  #[test]
  fn test_type_from_path_crashtest() {
    use std::path::Path;

    assert_eq!(
      TestType::from_path(Path::new("crashtest.html")),
      TestType::Crashtest
    );
    assert_eq!(
      TestType::from_path(Path::new("crash-001.html")),
      TestType::Crashtest
    );
  }

  #[test]
  fn test_type_from_path_manual() {
    use std::path::Path;

    assert_eq!(TestType::from_path(Path::new("manual-test.html")), TestType::Manual);
  }

  // =========================================================================
  // TestResult Tests
  // =========================================================================

  #[test]
  fn test_result_pass() {
    let metadata = TestMetadata::from_path(PathBuf::from("test.html"));
    let result = TestResult::pass(metadata, Duration::from_millis(100));

    assert_eq!(result.status, TestStatus::Pass);
    assert_eq!(result.duration, Duration::from_millis(100));
    assert!(result.message.is_none());
  }

  #[test]
  fn test_result_fail() {
    let metadata = TestMetadata::from_path(PathBuf::from("test.html"));
    let result = TestResult::fail(metadata, Duration::from_millis(50), "Image mismatch");

    assert_eq!(result.status, TestStatus::Fail);
    assert_eq!(result.message, Some("Image mismatch".to_string()));
  }

  #[test]
  fn test_result_error() {
    let metadata = TestMetadata::from_path(PathBuf::from("test.html"));
    let result = TestResult::error(metadata, Duration::from_millis(10), "Parse error");

    assert_eq!(result.status, TestStatus::Error);
    assert_eq!(result.message, Some("Parse error".to_string()));
  }

  #[test]
  fn test_result_skip() {
    let metadata = TestMetadata::from_path(PathBuf::from("test.html"));
    let result = TestResult::skip(metadata, "Not supported");

    assert_eq!(result.status, TestStatus::Skip);
    assert_eq!(result.duration, Duration::ZERO);
    assert_eq!(result.message, Some("Not supported".to_string()));
  }

  #[test]
  fn test_result_timeout() {
    let metadata = TestMetadata::from_path(PathBuf::from("test.html"));
    let result = TestResult::timeout(metadata, Duration::from_secs(30));

    assert_eq!(result.status, TestStatus::Timeout);
    assert!(result.message.unwrap().contains("timed out"));
  }

  #[test]
  fn test_result_with_images() {
    let metadata = TestMetadata::from_path(PathBuf::from("test.html"));
    let result = TestResult::pass(metadata, Duration::from_millis(100))
      .with_images(vec![1, 2, 3], vec![4, 5, 6]);

    assert!(result.rendered_image.is_some());
    assert!(result.expected_image.is_some());
    assert_eq!(result.rendered_image.unwrap(), vec![1, 2, 3]);
    assert_eq!(result.expected_image.unwrap(), vec![4, 5, 6]);
  }

  #[test]
  fn test_result_with_diff() {
    let metadata = TestMetadata::from_path(PathBuf::from("test.html"));
    let mut rendered = image::RgbaImage::from_pixel(10, 20, image::Rgba([0, 0, 0, 255]));
    rendered.put_pixel(0, 0, image::Rgba([10, 0, 0, 255]));
    let expected = image::RgbaImage::from_pixel(10, 20, image::Rgba([0, 0, 0, 255]));

    let rendered_png = fastrender::image_compare::encode_png(&rendered).unwrap();
    let expected_png = fastrender::image_compare::encode_png(&expected).unwrap();
    let diff = super::compare_images(&rendered_png, &expected_png, &CompareConfig::strict()).unwrap();

    let result = TestResult::pass(metadata, Duration::from_millis(100)).with_diff(&diff);

    assert_eq!(result.pixel_diff, Some(1));
    assert_eq!(result.diff_percentage, Some(0.5));
    assert_eq!(result.max_channel_diff, Some(10));
    assert_eq!(result.perceptual_distance.is_some(), true);
    assert_eq!(result.first_mismatch, Some((0, 0)));
    assert_eq!(
      result.first_mismatch_rgba,
      Some(([10, 0, 0, 255], [0, 0, 0, 255]))
    );
    assert_eq!(result.actual_dimensions, Some((10, 20)));
    assert_eq!(result.expected_dimensions, Some((10, 20)));
  }

  #[test]
  fn test_wpt_env_ignore_alpha_is_reflected_in_config() {
    let harness = HarnessConfig::default();
    let vars = HashMap::from([("WPT_IGNORE_ALPHA".to_string(), "1".to_string())]);
    let compare = harness.compare_config_from_env_map(&vars).unwrap();
    assert!(!compare.compare_alpha);
  }

  #[test]
  fn test_wpt_env_max_perceptual_distance_is_reflected_in_config() {
    let harness = HarnessConfig::default();
    let vars =
      HashMap::from([("WPT_MAX_PERCEPTUAL_DISTANCE".to_string(), "0.123".to_string())]);
    let compare = harness.compare_config_from_env_map(&vars).unwrap();
    assert_eq!(compare.max_perceptual_distance, Some(0.123));
  }

  // =========================================================================
  // SuiteResult Tests
  // =========================================================================

  #[test]
  fn test_suite_result_counts() {
    let mut suite = SuiteResult::new("test-suite");

    suite.add_result(TestResult::pass(
      TestMetadata::from_path(PathBuf::from("test1.html")),
      Duration::from_millis(100),
    ));
    suite.add_result(TestResult::pass(
      TestMetadata::from_path(PathBuf::from("test2.html")),
      Duration::from_millis(100),
    ));
    suite.add_result(TestResult::fail(
      TestMetadata::from_path(PathBuf::from("test3.html")),
      Duration::from_millis(100),
      "Failed",
    ));
    suite.add_result(TestResult::skip(
      TestMetadata::from_path(PathBuf::from("test4.html")),
      "Skipped",
    ));
    suite.add_result(TestResult::error(
      TestMetadata::from_path(PathBuf::from("test5.html")),
      Duration::from_millis(100),
      "Error",
    ));
    suite.finalize();

    assert_eq!(suite.total(), 5);
    assert_eq!(suite.passed(), 2);
    assert_eq!(suite.failed(), 1);
    assert_eq!(suite.skipped(), 1);
    assert_eq!(suite.errors(), 1);
    assert_eq!(suite.pass_rate(), 40.0); // 2 out of 5
  }

  #[test]
  fn test_suite_result_success() {
    let mut suite = SuiteResult::new("passing-suite");

    suite.add_result(TestResult::pass(
      TestMetadata::from_path(PathBuf::from("test1.html")),
      Duration::from_millis(100),
    ));
    suite.add_result(TestResult::skip(
      TestMetadata::from_path(PathBuf::from("test2.html")),
      "Skipped",
    ));

    assert!(suite.is_success()); // Pass and Skip are both success
  }

  #[test]
  fn test_suite_result_failure() {
    let mut suite = SuiteResult::new("failing-suite");

    suite.add_result(TestResult::pass(
      TestMetadata::from_path(PathBuf::from("test1.html")),
      Duration::from_millis(100),
    ));
    suite.add_result(TestResult::fail(
      TestMetadata::from_path(PathBuf::from("test2.html")),
      Duration::from_millis(100),
      "Failed",
    ));

    assert!(!suite.is_success()); // Has a failure
  }

  #[test]
  fn test_suite_result_display() {
    let mut suite = SuiteResult::new("test-suite");
    suite.add_result(TestResult::pass(
      TestMetadata::from_path(PathBuf::from("test.html")),
      Duration::from_millis(100),
    ));
    suite.finalize();

    let display = format!("{}", suite);
    assert!(display.contains("Suite: test-suite"));
    assert!(display.contains("Total: 1"));
    assert!(display.contains("Passed: 1"));
  }

  // =========================================================================
  // HarnessConfig Tests
  // =========================================================================

  #[test]
  fn test_harness_config_default() {
    let config = HarnessConfig::default();

    assert_eq!(config.test_dir, PathBuf::from("tests/wpt/tests"));
    assert_eq!(config.expected_dir, PathBuf::from("tests/wpt/expected"));
    assert_eq!(config.pixel_tolerance, 0);
    assert_eq!(config.max_diff_percentage, 0.0);
    assert_eq!(config.default_timeout_ms, 30000);
    assert!(!config.fail_fast);
    assert!(!config.parallel);
    assert!(!config.update_expected);
    assert!(config.manifest_path.is_none());
    assert!(config.write_report);
    assert_eq!(config.discovery_mode, DiscoveryMode::ManifestWithFallback);
    assert!(config.font_dirs.is_empty());
  }

  #[test]
  fn test_harness_config_with_test_dir() {
    let config = HarnessConfig::with_test_dir("custom/tests");

    assert_eq!(config.test_dir, PathBuf::from("custom/tests"));
    assert_eq!(config.expected_dir, PathBuf::from("custom/expected"));
  }

  #[test]
  fn test_harness_config_builder_methods() {
    let config = HarnessConfig::default()
      .with_tolerance(10)
      .with_max_diff(0.5)
      .fail_fast()
      .parallel(8)
      .with_filter("css")
      .update_expected()
      .with_manifest("custom/manifest.toml")
      .without_report()
      .with_discovery_mode(DiscoveryMode::MetadataOnly)
      .with_font_dir("fonts/ci");

    assert_eq!(config.pixel_tolerance, 10);
    assert_eq!(config.max_diff_percentage, 0.5);
    assert!(config.fail_fast);
    assert!(config.parallel);
    assert_eq!(config.workers, 8);
    assert_eq!(config.filter, Some("css".to_string()));
    assert!(config.update_expected);
    assert_eq!(
      config.manifest_path,
      Some(PathBuf::from("custom/manifest.toml"))
    );
    assert!(!config.write_report);
    assert_eq!(config.discovery_mode, DiscoveryMode::MetadataOnly);
    assert_eq!(config.font_dirs, vec![PathBuf::from("fonts/ci")]);
  }

  // =========================================================================
  // TestStatus Tests
  // =========================================================================

  #[test]
  fn test_status_success() {
    assert!(TestStatus::Pass.is_success());
    assert!(TestStatus::Skip.is_success());
    assert!(!TestStatus::Fail.is_success());
    assert!(!TestStatus::Error.is_success());
    assert!(!TestStatus::Timeout.is_success());
  }

  #[test]
  fn test_status_failure() {
    assert!(!TestStatus::Pass.is_failure());
    assert!(!TestStatus::Skip.is_failure());
    assert!(TestStatus::Fail.is_failure());
    assert!(TestStatus::Error.is_failure());
    assert!(TestStatus::Timeout.is_failure());
  }

  #[test]
  fn test_status_display() {
    assert_eq!(format!("{}", TestStatus::Pass), "PASS");
    assert_eq!(format!("{}", TestStatus::Fail), "FAIL");
    assert_eq!(format!("{}", TestStatus::Error), "ERROR");
    assert_eq!(format!("{}", TestStatus::Skip), "SKIP");
    assert_eq!(format!("{}", TestStatus::Timeout), "TIMEOUT");
  }

  // =========================================================================
  // AssertionResult Tests
  // =========================================================================

  #[test]
  fn test_assertion_result_variants() {
    let pass = AssertionResult::Pass;
    assert!(pass.is_pass());
    assert!(!pass.is_fail());
    assert!(!pass.is_error());

    let fail = AssertionResult::Fail("Failed".to_string());
    assert!(!fail.is_pass());
    assert!(fail.is_fail());
    assert!(!fail.is_error());

    let error = AssertionResult::Error("Error".to_string());
    assert!(!error.is_pass());
    assert!(!error.is_fail());
    assert!(error.is_error());
  }

  #[test]
  fn test_assertion_result_display() {
    assert_eq!(format!("{}", AssertionResult::Pass), "PASS");
    assert!(format!("{}", AssertionResult::Fail("msg".to_string())).contains("FAIL"));
    assert!(format!("{}", AssertionResult::Error("msg".to_string())).contains("ERROR"));
  }

  fn write_solid_png(path: &Path, width: u32, height: u32, color: [u8; 4]) {
    let image = RgbaImage::from_pixel(width, height, Rgba(color));
    let mut buffer = Vec::new();
    PngEncoder::new(&mut buffer)
      .write_image(image.as_raw(), width, height, ColorType::Rgba8.into())
      .unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, buffer).unwrap();
  }

  #[test]
  fn wpt_report_writes_without_artifact_pngs_when_disabled() {
    let temp = TempDir::new().unwrap();
    let suite_dir = temp.path();

    std::fs::write(
      suite_dir.join("crashtest.html"),
      "<!doctype html><html><body>ok</body></html>",
    )
    .unwrap();

    let output_dir = suite_dir.join("out");
    let renderer = super::create_test_renderer();
    let mut config = HarnessConfig::default();
    config.test_dir = suite_dir.to_path_buf();
    config.expected_dir = suite_dir.join("expected");
    config.output_dir = output_dir.clone();
    config.save_rendered = false;
    config.save_diffs = false;
    config.write_report = true;
    config.discovery_mode = DiscoveryMode::MetadataOnly;

    let mut runner = WptRunner::with_config(renderer, config);
    let results = runner.run_suite(suite_dir);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, TestStatus::Pass);

    assert!(output_dir.join("report.html").exists());

    let mut saw_artifact_png = false;
    for entry in WalkDir::new(&output_dir).into_iter().flatten() {
      if entry.file_type().is_file() {
        let name = entry.file_name().to_string_lossy();
        if name == "actual.png" || name == "diff.png" {
          saw_artifact_png = true;
          break;
        }
      }
    }
    assert!(
      !saw_artifact_png,
      "expected no actual.png/diff.png artifacts, found some under {:?}",
      output_dir
    );
  }

  #[test]
  fn wpt_writes_artifacts_for_failures_when_enabled() {
    let temp = TempDir::new().unwrap();
    let suite_dir = temp.path();

    std::fs::write(
      suite_dir.join("test.html"),
      r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; width: 100%; height: 100%; background: rgb(0, 255, 0); }
    </style>
  </head>
  <body></body>
</html>"#,
    )
    .unwrap();
    std::fs::write(suite_dir.join("test.ini"), "viewport: 10x10\n").unwrap();

    let expected_dir = suite_dir.join("expected");
    write_solid_png(&expected_dir.join("test.png"), 10, 10, [255, 0, 0, 255]);

    let output_dir = suite_dir.join("out");
    let renderer = super::create_test_renderer();
    let mut config = HarnessConfig::default();
    config.test_dir = suite_dir.to_path_buf();
    config.expected_dir = expected_dir;
    config.output_dir = output_dir.clone();
    config.save_rendered = true;
    config.save_diffs = true;
    config.write_report = true;
    config.discovery_mode = DiscoveryMode::MetadataOnly;

    let mut runner = WptRunner::with_config(renderer, config);
    let results = runner.run_suite(suite_dir);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, TestStatus::Fail);

    let base = output_dir.join("test");
    assert!(base.join("expected.png").exists());
    assert!(base.join("actual.png").exists());
    assert!(base.join("diff.png").exists());
  }
}
