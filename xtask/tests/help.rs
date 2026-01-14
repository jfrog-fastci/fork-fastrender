use std::process::Command;

#[test]
fn help_lists_commands() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .arg("--help")
    .output()
    .expect("run xtask --help");

  assert!(
    output.status.success(),
    "xtask help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  // Clap may choose to render the command list in either one-line or wrapped form depending on
  // terminal width and the longest subcommand name. Count the canonical `js` command entry by
  // matching the exact line.
  let js_count = stdout.matches("\n  js\n").count();
  assert!(
    stdout.contains("render-page")
      && stdout.contains("page-loop")
      && stdout.contains("update-goldens")
      && stdout.contains("diff-renders")
      && stdout.contains("freeze-page-fixture")
      && stdout.contains("chrome-baseline-fixtures")
      && stdout.contains("fixture-chrome-diff")
      && stdout.contains("fixture-determinism")
      && stdout.contains("analyze-lazy-loading")
      && stdout.contains("refresh-progress-accuracy")
      && stdout.contains("capture-accuracy-fixtures")
      && stdout.contains("pageset")
      && stdout.contains("pageset-diff")
      && stdout.contains("pageset-triage")
      && stdout.contains("\n  js\n")
      && stdout.contains("perf-smoke")
      && stdout.contains("ui-perf-smoke")
      && stdout.contains("lint-no-merge-conflicts")
      && stdout.contains("validate-page-fixtures")
      && stdout.contains("recapture-page-fixtures")
      && stdout.contains("import-page-fixture"),
    "help output should mention available subcommands; got:\n{stdout}"
  );

  assert_eq!(
    js_count, 1,
    "expected exactly one `js` subcommand in `xtask --help` output; got {js_count}.\n{stdout}"
  );
}

#[test]
fn import_page_fixture_help_mentions_media_flags() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["import-page-fixture", "--help"])
    .output()
    .expect("run xtask import-page-fixture --help");

  assert!(
    output.status.success(),
    "import-page-fixture help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--include-media")
      && stdout.contains("--media-max-bytes")
      && stdout.contains("--media-max-file-bytes"),
    "import-page-fixture help should mention media vendoring flags; got:\n{stdout}"
  );
}

#[test]
fn update_goldens_help_lists_pages_suite() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["update-goldens", "--help"])
    .output()
    .expect("run xtask update-goldens --help");

  assert!(
    output.status.success(),
    "update-goldens help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("Possible values:") && stdout.contains("- pages:"),
    "update-goldens help should list the `pages` suite; got:\n{stdout}"
  );
}

#[test]
fn ui_perf_smoke_help_mentions_rayon_threads_flag() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["ui-perf-smoke", "--help"])
    .output()
    .expect("run xtask ui-perf-smoke --help");

  assert!(
    output.status.success(),
    "ui-perf-smoke help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--rayon-threads"),
    "ui-perf-smoke help should mention --rayon-threads; got:\n{stdout}"
  );
}

#[test]
fn js_help_lists_subcommands() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["js", "--help"])
    .output()
    .expect("run xtask js --help");

  assert!(output.status.success(), "js help should exit successfully");

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("\n  test262 ")
      && stdout.contains("test262-parser")
      && stdout.contains("wpt-dom"),
    "help output should list test262, test262-parser, and wpt-dom; got:\n{stdout}"
  );
}

#[test]
fn js_test262_help_mentions_flags() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["js", "test262", "--help"])
    .output()
    .expect("run xtask js test262 --help");

  assert!(
    output.status.success(),
    "js test262 help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--suite")
      && stdout.contains("--harness")
      && stdout.contains("--manifest")
      && stdout.contains("--shard")
      && stdout.contains("--timeout-secs")
      && stdout.contains("--fail-on")
      && stdout.contains("--report")
      && stdout.contains("--filter")
      && stdout.contains("--harness"),
    "help output should mention key flags; got:\n{stdout}"
  );
}

#[test]
fn js_test262_parser_help_mentions_flags() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["js", "test262-parser", "--help"])
    .output()
    .expect("run xtask js test262-parser --help");

  assert!(
    output.status.success(),
    "js test262-parser help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--manifest")
      && stdout.contains("--shard")
      && stdout.contains("--timeout-secs")
      && stdout.contains("--fail-on")
      && stdout.contains("--report")
      && stdout.contains("--test262-dir"),
    "help output should mention key flags; got:\n{stdout}"
  );
}

#[test]
fn js_wpt_dom_help_mentions_flags() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["js", "wpt-dom", "--help"])
    .output()
    .expect("run xtask js wpt-dom --help");

  assert!(
    output.status.success(),
    "js wpt-dom help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--suite")
      && stdout.contains("--wpt-root")
      && stdout.contains("--manifest")
      && stdout.contains("--shard")
      && stdout.contains("--filter")
      && stdout.contains("--timeout-ms")
      && stdout.contains("--timeout-secs")
      && stdout.contains("--long-timeout-ms")
      && stdout.contains("--long-timeout-secs")
      && stdout.contains("--fail-on")
      && stdout.contains("--report")
      && stdout.contains("--backend"),
    "help output should mention key flags; got:\n{stdout}"
  );
}

#[test]
fn browser_help_mentions_instrumentation_flags() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["browser", "--help"])
    .output()
    .expect("run xtask browser --help");

  assert!(
    output.status.success(),
    "browser help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--hud")
      && stdout.contains("--no-hud")
      && stdout.contains("--perf-log")
      && stdout.contains("--perf-log-out")
      && stdout.contains("--trace-out"),
    "browser help should mention instrumentation flags; got:\n{stdout}"
  );
}

#[test]
fn js_wpt_dom_smoke_sync_pass_reports_pass() {
  let dir = tempfile::TempDir::new().expect("tempdir");
  let report_path = dir.path().join("wpt_dom_report.json");

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args([
      "js",
      "wpt-dom",
      "--filter",
      "smoke/sync-pass.html",
      "--report",
    ])
    .arg(&report_path)
    .output()
    .expect("run xtask js wpt-dom");

  assert!(
    output.status.success(),
    "xtask js wpt-dom should exit successfully; stderr:\n{}",
    String::from_utf8_lossy(&output.stderr)
  );

  let raw = std::fs::read_to_string(&report_path).expect("read report");
  let json: serde_json::Value = serde_json::from_str(&raw).expect("parse report JSON");

  let results = json["results"]
    .as_array()
    .expect("results should be an array");
  assert_eq!(results.len(), 1, "expected exactly one filtered result");
  assert_eq!(results[0]["id"], "smoke/sync-pass.html");
  assert_eq!(results[0]["outcome"], "passed");
  assert_eq!(json["summary"]["passed"], 1);
  assert_eq!(json["summary"]["failed"], 0);
  assert_eq!(json["summary"]["errored"], 0);
  assert_eq!(json["summary"]["timed_out"], 0);
}

#[test]
fn chrome_baseline_fixtures_help_mentions_flags() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["chrome-baseline-fixtures", "--help"])
    .output()
    .expect("run xtask chrome-baseline-fixtures --help");

  assert!(
    output.status.success(),
    "chrome-baseline-fixtures help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--fixture-dir")
      && stdout.contains("--fixtures-dir")
      && stdout.contains("--fixtures-root")
      && stdout.contains("--out-dir")
      && stdout.contains("--fixtures")
      && stdout.contains("--shard")
      && stdout.contains("--chrome")
      && stdout.contains("--chrome-dir")
      && stdout.contains("--viewport")
      && stdout.contains("--dpr")
      && stdout.contains("--media")
      && stdout.contains("--timeout")
      && stdout.contains("--js"),
    "help output should mention key flags; got:\n{stdout}"
  );
}

#[test]
fn fixture_chrome_diff_help_mentions_flags() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["fixture-chrome-diff", "--help"])
    .output()
    .expect("run xtask fixture-chrome-diff --help");

  assert!(
    output.status.success(),
    "fixture-chrome-diff help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--fixtures-dir")
      && stdout.contains("--out-dir")
      && stdout.contains("--fixtures")
      && stdout.contains("--from-progress")
      && stdout.contains("--only-failures")
      && stdout.contains("--top-worst-accuracy")
      && stdout.contains("--min-diff-percent")
      && stdout.contains("--skip-missing-fixtures")
      && stdout.contains("--all-fixtures")
      && stdout.contains("--shard")
      && stdout.contains("--jobs")
      && stdout.contains("--write-snapshot")
      && stdout.contains("--overlay")
      && stdout.contains("--viewport")
      && stdout.contains("--dpr")
      && stdout.contains("--media")
      && stdout.contains("--timeout")
      && stdout.contains("--js")
      && stdout.contains("--tolerance")
      && stdout.contains("--max-diff-percent")
      && stdout.contains("--max-perceptual-distance")
      && stdout.contains("--sort-by")
      && stdout.contains("--ignore-alpha")
      && stdout.contains("--fail-on-differences")
      && stdout.contains("--debug")
      && stdout.contains("--no-build")
      && stdout.contains("--no-fastrender")
      && stdout.contains("--diff-only")
      && stdout.contains("--chrome")
      && stdout.contains("--chrome-dir")
      && stdout.contains("--no-chrome")
      && stdout.contains("--require-chrome-metadata"),
    "help output should mention key flags; got:\n{stdout}"
  );
}

#[test]
fn fixture_determinism_help_mentions_flags() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["fixture-determinism", "--help"])
    .output()
    .expect("run xtask fixture-determinism --help");

  assert!(
    output.status.success(),
    "fixture-determinism help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--fixtures-dir")
      && stdout.contains("--out-dir")
      && stdout.contains("--fixtures")
      && stdout.contains("--shard")
      && stdout.contains("--repeat")
      && stdout.contains("--viewport")
      && stdout.contains("--dpr")
      && stdout.contains("--media")
      && stdout.contains("--timeout")
      && stdout.contains("--ignore-alpha")
      && stdout.contains("--allow-differences")
      && stdout.contains("--debug")
      && stdout.contains("--no-build"),
    "help output should mention key flags; got:\n{stdout}"
  );
}

#[test]
fn page_loop_help_mentions_flags() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["page-loop", "--help"])
    .output()
    .expect("run xtask page-loop --help");

  assert!(
    output.status.success(),
    "page-loop help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--fixture")
      && stdout.contains("--pageset")
      && stdout.contains("--from-progress")
      && stdout.contains("--top-worst-accuracy")
      && stdout.contains("--top-slowest")
      && stdout.contains("--only-failures")
      && stdout.contains("--hotspot")
      && stdout.contains("--viewport")
      && stdout.contains("--dpr")
      && stdout.contains("--jobs")
      && stdout.contains("--timeout")
      && stdout.contains("--media")
      && stdout.contains("--compat-profile")
      && stdout.contains("--dom-compat")
      && stdout.contains("--out-dir")
      && stdout.contains("--write-snapshot")
      && stdout.contains("--overlay")
      && stdout.contains("--inspect-dump-json")
      && stdout.contains("--inspect-filter-selector")
      && stdout.contains("--inspect-filter-id")
      && stdout.contains("--inspect-dump-custom-properties")
      && stdout.contains("--inspect-custom-property-prefix")
      && stdout.contains("--inspect-custom-properties-limit")
      && stdout.contains("--chrome")
      && stdout.contains("--no-chrome")
      && stdout.contains("--debug")
      && stdout.contains("--dry-run"),
    "page-loop help should mention key flags; got:\n{stdout}"
  );
}

#[test]
fn refresh_progress_accuracy_help_mentions_flags() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["refresh-progress-accuracy", "--help"])
    .output()
    .expect("run xtask refresh-progress-accuracy --help");

  assert!(
    output.status.success(),
    "refresh-progress-accuracy help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--progress-dir")
      && stdout.contains("--out-dir")
      && stdout.contains("--fixtures")
      && stdout.contains("--from-progress")
      && stdout.contains("--only-failures")
      && stdout.contains("--top-worst-accuracy")
      && stdout.contains("--tolerance")
      && stdout.contains("--max-diff-percent")
      && stdout.contains("--ignore-alpha")
      && stdout.contains("--max-perceptual-distance")
      && stdout.contains("--dry-run")
      && stdout.contains("--print-top-worst"),
    "help output should mention key flags; got:\n{stdout}"
  );
}

#[test]
fn pageset_help_mentions_disk_cache_flag() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["pageset", "--help"])
    .output()
    .expect("run xtask pageset --help");

  assert!(
    output.status.success(),
    "pageset help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--no-disk-cache") && stdout.contains("--disk-cache"),
    "pageset help should mention disk cache enable/disable flags; got:\n{stdout}"
  );
}

#[test]
fn pageset_help_mentions_filters() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["pageset", "--help"])
    .output()
    .expect("run xtask pageset --help");

  assert!(
    output.status.success(),
    "xtask pageset help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--shard") && stdout.contains("--pages"),
    "pageset help should mention sharding and page filters; got:\n{stdout}"
  );
}

#[test]
fn pageset_help_mentions_font_flags() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["pageset", "--help"])
    .output()
    .expect("run xtask pageset --help");

  assert!(
    output.status.success(),
    "xtask pageset help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--system-fonts")
      && stdout.contains("no-bundled-fonts")
      && stdout.contains("--bundled-fonts"),
    "pageset help should mention bundled/system font toggles; got:\n{stdout}"
  );
}

#[test]
fn pageset_help_mentions_cascade_diagnostics() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["pageset", "--help"])
    .output()
    .expect("run xtask pageset --help");

  assert!(
    output.status.success(),
    "xtask pageset help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--cascade-diagnostics"),
    "pageset help should mention cascade diagnostics reruns; got:\n{stdout}"
  );
}

#[test]
fn pageset_help_mentions_allow_http_error_status() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["pageset", "--help"])
    .output()
    .expect("run xtask pageset --help");

  assert!(
    output.status.success(),
    "xtask pageset help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--allow-http-error-status"),
    "pageset help should mention --allow-http-error-status; got:\n{stdout}"
  );
}

#[test]
fn pageset_help_mentions_allow_collisions() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["pageset", "--help"])
    .output()
    .expect("run xtask pageset --help");

  assert!(
    output.status.success(),
    "xtask pageset help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--allow-collisions"),
    "pageset help should mention --allow-collisions; got:\n{stdout}"
  );
}

#[test]
fn pageset_help_mentions_timings() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["pageset", "--help"])
    .output()
    .expect("run xtask pageset --help");

  assert!(
    output.status.success(),
    "xtask pageset help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--timings"),
    "pageset help should mention --timings; got:\n{stdout}"
  );
}

#[test]
fn pageset_help_mentions_cache_dir() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["pageset", "--help"])
    .output()
    .expect("run xtask pageset --help");

  assert!(
    output.status.success(),
    "xtask pageset help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--cache-dir"),
    "pageset help should mention --cache-dir; got:\n{stdout}"
  );
}

#[test]
fn pageset_help_mentions_accuracy_flags() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["pageset", "--help"])
    .output()
    .expect("run xtask pageset --help");

  assert!(
    output.status.success(),
    "xtask pageset help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--accuracy")
      && stdout.contains("--accuracy-baseline")
      && stdout.contains("--accuracy-baseline-dir")
      && stdout.contains("--accuracy-tolerance")
      && stdout.contains("--accuracy-max-diff-percent")
      && stdout.contains("--accuracy-diff-dir"),
    "pageset help should mention accuracy flags; got:\n{stdout}"
  );
}

#[test]
fn pageset_diff_help_mentions_accuracy_regression_flags() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["pageset-diff", "--help"])
    .output()
    .expect("run xtask pageset-diff --help");

  assert!(
    output.status.success(),
    "pageset-diff help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--fail-on-accuracy-regression")
      && stdout.contains("--accuracy-regression-threshold-percent"),
    "pageset-diff help should mention accuracy regression gating flags; got:\n{stdout}"
  );
}

#[test]
fn diff_renders_help_mentions_ignore_alpha() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["diff-renders", "--help"])
    .output()
    .expect("run xtask diff-renders --help");

  assert!(
    output.status.success(),
    "diff-renders help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--ignore-alpha") && stdout.contains("--fail-on-differences"),
    "diff-renders help should mention --ignore-alpha and --fail-on-differences; got:\n{stdout}"
  );
}

#[test]
fn pageset_help_mentions_capture_missing_failure_fixtures() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["pageset", "--help"])
    .output()
    .expect("run xtask pageset --help");

  assert!(
    output.status.success(),
    "xtask pageset help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--capture-missing-failure-fixtures")
      && stdout.contains("--capture-missing-failure-fixtures-out-dir")
      && stdout.contains("--capture-missing-failure-fixtures-allow-missing-resources")
      && stdout.contains("--capture-missing-failure-fixtures-overwrite")
      && stdout.contains("--capture-worst-accuracy-fixtures")
      && stdout.contains("--capture-worst-accuracy-fixtures-out-dir")
      && stdout.contains("--capture-worst-accuracy-fixtures-min-diff-percent")
      && stdout.contains("--capture-worst-accuracy-fixtures-top")
      && stdout.contains("--capture-worst-accuracy-fixtures-allow-missing-resources")
      && stdout.contains("--capture-worst-accuracy-fixtures-overwrite"),
    "pageset help should mention the fixture capture flags; got:\n{stdout}"
  );
}

#[test]
fn pageset_help_mentions_refresh_flag() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["pageset", "--help"])
    .output()
    .expect("run xtask pageset --help");

  assert!(
    output.status.success(),
    "xtask pageset help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--refresh"),
    "pageset help should mention --refresh; got:\n{stdout}"
  );
}

#[test]
fn update_pageset_guardrails_help_mentions_strategy_flag() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["update-pageset-guardrails", "--help"])
    .output()
    .expect("run xtask update-pageset-guardrails --help");

  assert!(
    output.status.success(),
    "update-pageset-guardrails help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--strategy"),
    "update-pageset-guardrails help should mention the selection strategy; got:\n{stdout}"
  );
}

#[test]
fn update_pageset_guardrails_help_mentions_cache_dir_alias() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["update-pageset-guardrails", "--help"])
    .output()
    .expect("run xtask update-pageset-guardrails --help");

  assert!(
    output.status.success(),
    "update-pageset-guardrails help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("aliases: --cache-dir"),
    "help output should mention the --cache-dir alias for --asset-cache-dir; got:\n{stdout}"
  );
}

#[test]
fn update_pageset_timeouts_alias_help_mentions_strategy_flag() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["update-pageset-timeouts", "--help"])
    .output()
    .expect("run xtask update-pageset-timeouts --help");

  assert!(
    output.status.success(),
    "update-pageset-timeouts help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--strategy"),
    "update-pageset-timeouts help should mention the selection strategy; got:\n{stdout}"
  );
}

#[test]
fn recapture_page_fixtures_help_mentions_cache_dir_alias() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["recapture-page-fixtures", "--help"])
    .output()
    .expect("run xtask recapture-page-fixtures --help");

  assert!(
    output.status.success(),
    "recapture-page-fixtures help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("aliases: --cache-dir"),
    "help output should mention the --cache-dir alias for --asset-cache-dir; got:\n{stdout}"
  );
}

#[test]
fn update_pageset_guardrails_budgets_help_mentions_multiplier_flag() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["update-pageset-guardrails-budgets", "--help"])
    .output()
    .expect("run xtask update-pageset-guardrails-budgets --help");

  assert!(
    output.status.success(),
    "update-pageset-guardrails-budgets help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--multiplier"),
    "update-pageset-guardrails-budgets help should mention --multiplier; got:\n{stdout}"
  );
}

#[test]
fn update_pageset_timeout_budgets_alias_help_mentions_multiplier_flag() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["update-pageset-timeout-budgets", "--help"])
    .output()
    .expect("run xtask update-pageset-timeout-budgets --help");

  assert!(
    output.status.success(),
    "update-pageset-timeout-budgets help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--multiplier"),
    "update-pageset-timeout-budgets help should mention --multiplier; got:\n{stdout}"
  );
}

#[test]
fn perf_smoke_help_mentions_suites_and_regression_flags() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["perf-smoke", "--help"])
    .output()
    .expect("run xtask perf-smoke --help");

  assert!(
    output.status.success(),
    "perf-smoke help should exit successfully"
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--suite")
      && stdout.contains("pageset-guardrails")
      && stdout.contains("--only")
      && stdout.contains("--baseline")
      && stdout.contains("--threshold")
      && stdout.contains("--count-threshold")
      && stdout.contains("--fail-on-regression")
      && stdout.contains("--fail-fast")
      && stdout.contains("--fail-on-failure")
      && stdout.contains("--no-fail-on-failure")
      && stdout.contains("--fail-on-missing-fixtures")
      && stdout.contains("--allow-missing-fixtures")
      && stdout.contains("--fail-on-budget")
      && stdout.contains("--fail-on-fetch-errors")
      && stdout.contains("--isolate")
      && stdout.contains("--no-isolate")
      && stdout.contains("--top")
      && stdout.contains("--output")
      && stdout.contains("--debug")
      && stdout.contains("EXTRA"),
    "perf-smoke help should mention suite selection and regression gating flags; got:\n{stdout}"
  );
}
