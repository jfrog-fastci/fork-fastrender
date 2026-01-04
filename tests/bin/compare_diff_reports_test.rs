use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::Command;

fn output_text(bytes: &[u8]) -> String {
  String::from_utf8_lossy(bytes).to_string()
}

fn write_json(path: &Path, value: &Value) {
  fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
}

fn compare_cmd(tmp_dir: &Path) -> Command {
  let mut cmd = Command::new(env!("CARGO_BIN_EXE_compare_diff_reports"));
  cmd.current_dir(tmp_dir);
  cmd
}

fn basic_report(entries: Vec<Value>) -> Value {
  json!({
    "before_dir": "chrome",
    "after_dir": "fastrender",
    "tolerance": 0,
    "max_diff_percent": 0.0,
    "ignore_alpha": false,
    "results": entries,
    "totals": {
      "discovered": entries.len(),
      "processed": entries.len(),
      "matches": 0,
      "within_threshold": 0,
      "differences": 0,
      "missing": 0,
      "errors": 0,
      "shard_skipped": 0
    }
  })
}

#[test]
fn compare_diff_reports_pairs_and_classifies_entries() {
  let tmp = tempfile::TempDir::new().expect("tempdir");
  let baseline_path = tmp.path().join("baseline.json");
  let new_path = tmp.path().join("new.json");
  let out_json = tmp.path().join("delta.json");
  let out_html = tmp.path().join("delta.html");

  let baseline = basic_report(vec![
    json!({
      "name": "a",
      "status": "diff",
      "metrics": {
        "pixel_diff": 10,
        "total_pixels": 100,
        "diff_percentage": 10.0,
        "perceptual_distance": 0.5
      }
    }),
    json!({
      "name": "b",
      "status": "diff",
      "metrics": {
        "pixel_diff": 0,
        "total_pixels": 100,
        "diff_percentage": 0.0,
        "perceptual_distance": 0.0
      }
    }),
    json!({
      "name": "c",
      "status": "missing_after",
      "error": "Missing in after input"
    }),
    json!({
      "name": "e",
      "status": "diff",
      "metrics": {
        "pixel_diff": 50,
        "total_pixels": 100,
        "diff_percentage": 50.0,
        "perceptual_distance": 0.9
      }
    }),
  ]);
  let new_report = basic_report(vec![
    json!({
      "name": "a",
      "status": "diff",
      "metrics": {
        "pixel_diff": 5,
        "total_pixels": 100,
        "diff_percentage": 5.0,
        "perceptual_distance": 0.25
      }
    }),
    json!({
      "name": "b",
      "status": "diff",
      "metrics": {
        "pixel_diff": 1,
        "total_pixels": 100,
        "diff_percentage": 1.0,
        "perceptual_distance": 0.1
      }
    }),
    json!({
      "name": "c",
      "status": "error",
      "error": "Diff failed"
    }),
    json!({
      "name": "d",
      "status": "diff",
      "metrics": {
        "pixel_diff": 2,
        "total_pixels": 100,
        "diff_percentage": 2.0,
        "perceptual_distance": 0.2
      }
    }),
  ]);

  write_json(&baseline_path, &baseline);
  write_json(&new_path, &new_report);

  let output = compare_cmd(tmp.path())
    .args([
      "--baseline",
      baseline_path.to_str().unwrap(),
      "--new",
      new_path.to_str().unwrap(),
      "--json",
      out_json.to_str().unwrap(),
      "--html",
      out_html.to_str().unwrap(),
    ])
    .output()
    .expect("run compare_diff_reports");

  assert!(
    output.status.success(),
    "expected success, got {:?}\nstdout:\n{}\nstderr:\n{}",
    output.status.code(),
    output_text(&output.stdout),
    output_text(&output.stderr),
  );
  assert!(out_json.exists(), "missing delta json");
  assert!(out_html.exists(), "missing delta html");

  let report: Value = serde_json::from_str(&fs::read_to_string(&out_json).unwrap()).unwrap();
  assert_eq!(report["schema_version"], 2);
  assert_eq!(
    report["baseline"]["report_json"],
    fs::canonicalize(&baseline_path).unwrap().display().to_string()
  );
  assert_eq!(
    report["new"]["report_json"],
    fs::canonicalize(&new_path).unwrap().display().to_string()
  );
  assert!(
    report["baseline"].get("report_html").is_none(),
    "did not expect report_html when no html path is provided"
  );
  assert!(
    report["new"].get("report_html").is_none(),
    "did not expect report_html when no html path is provided"
  );

  let aggregate = &report["aggregate"];
  assert_eq!(aggregate["paired_with_metrics"], 2);
  assert_eq!(aggregate["baseline"]["total_pixels"], 200);
  assert_eq!(aggregate["baseline"]["pixel_diff"], 10);
  assert_eq!(aggregate["new"]["total_pixels"], 200);
  assert_eq!(aggregate["new"]["pixel_diff"], 6);

  let baseline_weighted = aggregate["baseline"]["weighted_diff_percentage"]
    .as_f64()
    .expect("baseline weighted_diff_percentage");
  let new_weighted = aggregate["new"]["weighted_diff_percentage"]
    .as_f64()
    .expect("new weighted_diff_percentage");
  let delta_weighted = aggregate["delta"]["weighted_diff_percentage"]
    .as_f64()
    .expect("delta weighted_diff_percentage");
  assert!((baseline_weighted - 5.0).abs() < 1e-9);
  assert!((new_weighted - 3.0).abs() < 1e-9);
  assert!((delta_weighted + 2.0).abs() < 1e-9);

  let baseline_mean = aggregate["baseline"]["mean_diff_percentage"]
    .as_f64()
    .expect("baseline mean_diff_percentage");
  let new_mean = aggregate["new"]["mean_diff_percentage"]
    .as_f64()
    .expect("new mean_diff_percentage");
  let delta_mean = aggregate["delta"]["mean_diff_percentage"]
    .as_f64()
    .expect("delta mean_diff_percentage");
  assert!((baseline_mean - 5.0).abs() < 1e-9);
  assert!((new_mean - 3.0).abs() < 1e-9);
  assert!((delta_mean + 2.0).abs() < 1e-9);

  let baseline_perceptual = aggregate["baseline"]["mean_perceptual_distance"]
    .as_f64()
    .expect("baseline mean_perceptual_distance");
  let new_perceptual = aggregate["new"]["mean_perceptual_distance"]
    .as_f64()
    .expect("new mean_perceptual_distance");
  let delta_perceptual = aggregate["delta"]["mean_perceptual_distance"]
    .as_f64()
    .expect("delta mean_perceptual_distance");
  assert!((baseline_perceptual - 0.25).abs() < 1e-9);
  assert!((new_perceptual - 0.175).abs() < 1e-9);
  assert!((delta_perceptual + 0.075).abs() < 1e-9);

  let results = report["results"].as_array().expect("results array");

  let find = |name: &str| {
    results
      .iter()
      .find(|entry| entry["name"] == name)
      .unwrap_or_else(|| panic!("missing entry {name}"))
  };

  let a = find("a");
  assert_eq!(a["classification"], "improved");
  assert_eq!(a["diff_percentage_delta"].as_f64(), Some(-5.0));

  let b = find("b");
  assert_eq!(b["classification"], "regressed");
  assert_eq!(b["diff_percentage_delta"].as_f64(), Some(1.0));

  let c = find("c");
  assert_eq!(c["classification"], "regressed");
  assert!(c.get("diff_percentage_delta").is_none());

  let d = find("d");
  assert_eq!(d["classification"], "missing_in_baseline");

  let e = find("e");
  assert_eq!(e["classification"], "missing_in_new");
}

#[test]
fn compare_diff_reports_can_gate_on_regressions() {
  let tmp = tempfile::TempDir::new().expect("tempdir");
  let baseline_path = tmp.path().join("baseline.json");
  let new_path = tmp.path().join("new.json");

  let baseline = basic_report(vec![json!({
    "name": "a",
    "status": "diff",
    "metrics": {
      "pixel_diff": 0,
      "total_pixels": 100,
      "diff_percentage": 0.0,
      "perceptual_distance": 0.0
    }
  })]);
  let new_report = basic_report(vec![json!({
    "name": "a",
    "status": "diff",
    "metrics": {
      "pixel_diff": 1,
      "total_pixels": 100,
      "diff_percentage": 0.5,
      "perceptual_distance": 0.1
    }
  })]);

  write_json(&baseline_path, &baseline);
  write_json(&new_path, &new_report);

  let out_json_fail = tmp.path().join("delta_fail.json");
  let out_html_fail = tmp.path().join("delta_fail.html");
  let output = compare_cmd(tmp.path())
    .args([
      "--baseline",
      baseline_path.to_str().unwrap(),
      "--new",
      new_path.to_str().unwrap(),
      "--json",
      out_json_fail.to_str().unwrap(),
      "--html",
      out_html_fail.to_str().unwrap(),
      "--fail-on-regression",
      "--regression-threshold-percent",
      "0",
    ])
    .output()
    .expect("run compare_diff_reports");

  assert!(
    !output.status.success(),
    "expected failure exit code\nstdout:\n{}\nstderr:\n{}",
    output_text(&output.stdout),
    output_text(&output.stderr)
  );
  let stdout = output_text(&output.stdout);
  assert!(
    stdout.contains("Failing regressions: 1"),
    "missing failing regression count in stdout:\n{stdout}"
  );
  assert!(out_json_fail.exists(), "delta json should still be written");
  assert!(out_html_fail.exists(), "delta html should still be written");

  let report: Value =
    serde_json::from_str(&fs::read_to_string(&out_json_fail).expect("read delta json")).unwrap();
  assert_eq!(report["gating"]["fail_on_regression"], true);
  assert_eq!(report["gating"]["regression_threshold_percent"].as_f64(), Some(0.0));
  let results = report["results"].as_array().expect("results array");
  let entry = results.iter().find(|e| e["name"] == "a").expect("entry a");
  assert_eq!(entry["failing_regression"], true);
  let html = fs::read_to_string(&out_html_fail).expect("read delta html");
  assert!(html.contains("<strong>Gating:</strong>"), "missing Gating row:\n{html}");
  assert!(
    html.contains("<code>0.0000%</code>"),
    "missing gating threshold in html:\n{html}"
  );
  assert!(
    html.contains("Failing regressions: 1"),
    "missing failing regression count in html summary:\n{html}"
  );
  assert!(
    html.contains("class=\"regressed failing\""),
    "missing failing class in html:\n{html}"
  );

  let out_json_pass = tmp.path().join("delta_pass.json");
  let out_html_pass = tmp.path().join("delta_pass.html");
  let output = compare_cmd(tmp.path())
    .args([
      "--baseline",
      baseline_path.to_str().unwrap(),
      "--new",
      new_path.to_str().unwrap(),
      "--json",
      out_json_pass.to_str().unwrap(),
      "--html",
      out_html_pass.to_str().unwrap(),
      "--fail-on-regression",
      "--regression-threshold-percent",
      "1",
    ])
    .output()
    .expect("run compare_diff_reports");

  assert!(
    output.status.success(),
    "expected success with threshold\nstdout:\n{}\nstderr:\n{}",
    output_text(&output.stdout),
    output_text(&output.stderr)
  );
  let stdout = output_text(&output.stdout);
  assert!(
    stdout.contains("Failing regressions: 0"),
    "missing failing regression count in stdout:\n{stdout}"
  );

  let report: Value =
    serde_json::from_str(&fs::read_to_string(&out_json_pass).expect("read delta json")).unwrap();
  assert_eq!(report["gating"]["fail_on_regression"], true);
  assert_eq!(report["gating"]["regression_threshold_percent"].as_f64(), Some(1.0));
  let results = report["results"].as_array().expect("results array");
  let entry = results.iter().find(|e| e["name"] == "a").expect("entry a");
  assert!(
    entry.get("failing_regression").is_none(),
    "did not expect failing_regression field when regression is below threshold: {entry}"
  );
  let html = fs::read_to_string(&out_html_pass).expect("read delta html");
  assert!(html.contains("<strong>Gating:</strong>"), "missing Gating row:\n{html}");
  assert!(
    html.contains("<code>1.0000%</code>"),
    "missing gating threshold in html:\n{html}"
  );
  assert!(
    html.contains("Failing regressions: 0"),
    "missing failing regression count in html summary:\n{html}"
  );
  assert!(
    !html.contains("class=\"regressed failing\""),
    "did not expect failing class in html:\n{html}"
  );
}

#[test]
fn compare_diff_reports_gating_fails_on_missing_entries_in_new_report() {
  let tmp = tempfile::TempDir::new().expect("tempdir");
  let baseline_path = tmp.path().join("baseline.json");
  let new_path = tmp.path().join("new.json");
  let out_json = tmp.path().join("delta.json");
  let out_html = tmp.path().join("delta.html");

  let baseline = basic_report(vec![json!({
    "name": "a",
    "status": "diff",
    "metrics": {
      "pixel_diff": 1,
      "total_pixels": 100,
      "diff_percentage": 1.0,
      "perceptual_distance": 0.1
    }
  })]);
  let new_report = basic_report(vec![]);

  write_json(&baseline_path, &baseline);
  write_json(&new_path, &new_report);

  let output = compare_cmd(tmp.path())
    .args([
      "--baseline",
      baseline_path.to_str().unwrap(),
      "--new",
      new_path.to_str().unwrap(),
      "--json",
      out_json.to_str().unwrap(),
      "--html",
      out_html.to_str().unwrap(),
      "--fail-on-regression",
    ])
    .output()
    .expect("run compare_diff_reports");

  assert!(
    !output.status.success(),
    "expected failure exit code\nstdout:\n{}\nstderr:\n{}",
    output_text(&output.stdout),
    output_text(&output.stderr)
  );
  assert!(out_json.exists(), "delta json should still be written");
  assert!(out_html.exists(), "delta html should still be written");

  let report: Value = serde_json::from_str(&fs::read_to_string(&out_json).unwrap()).unwrap();
  let results = report["results"].as_array().expect("results array");
  let entry = results
    .iter()
    .find(|e| e["name"] == "a")
    .expect("missing entry a");
  assert_eq!(entry["classification"], "missing_in_new");
  assert_eq!(entry["failing_regression"], true);
}

#[test]
fn compare_diff_reports_requires_matching_config_by_default() {
  let tmp = tempfile::TempDir::new().expect("tempdir");
  let baseline_path = tmp.path().join("baseline.json");
  let new_path = tmp.path().join("new.json");
  let out_json_fail = tmp.path().join("delta_fail.json");
  let out_html_fail = tmp.path().join("delta_fail.html");
  let out_json_ok = tmp.path().join("delta_ok.json");
  let out_html_ok = tmp.path().join("delta_ok.html");

  let mut baseline = basic_report(vec![]);
  baseline["tolerance"] = json!(0);
  let mut new_report = basic_report(vec![]);
  new_report["tolerance"] = json!(5);

  write_json(&baseline_path, &baseline);
  write_json(&new_path, &new_report);

  let output = compare_cmd(tmp.path())
    .args([
      "--baseline",
      baseline_path.to_str().unwrap(),
      "--new",
      new_path.to_str().unwrap(),
      "--json",
      out_json_fail.to_str().unwrap(),
      "--html",
      out_html_fail.to_str().unwrap(),
    ])
    .output()
    .expect("run compare_diff_reports");

  assert!(
    !output.status.success(),
    "expected non-zero exit for config mismatch\nstdout:\n{}\nstderr:\n{}",
    output_text(&output.stdout),
    output_text(&output.stderr),
  );

  assert!(out_json_fail.exists(), "delta json should still be written");
  assert!(out_html_fail.exists(), "delta html should still be written");
  let report: Value =
    serde_json::from_str(&fs::read_to_string(&out_json_fail).expect("read delta json")).unwrap();
  assert_eq!(report["config_mismatches"].as_array().unwrap().len(), 1);
  assert_eq!(report["config_mismatches"][0]["field"], "tolerance");

  let output = compare_cmd(tmp.path())
    .args([
      "--baseline",
      baseline_path.to_str().unwrap(),
      "--new",
      new_path.to_str().unwrap(),
      "--json",
      out_json_ok.to_str().unwrap(),
      "--html",
      out_html_ok.to_str().unwrap(),
      "--allow-config-mismatch",
    ])
    .output()
    .expect("run compare_diff_reports");

  assert!(
    output.status.success(),
    "expected success when mismatch allowed\nstdout:\n{}\nstderr:\n{}",
    output_text(&output.stdout),
    output_text(&output.stderr)
  );

  let report: Value = serde_json::from_str(&fs::read_to_string(&out_json_ok).unwrap()).unwrap();
  assert_eq!(report["config_mismatches"].as_array().unwrap().len(), 1);
  assert_eq!(report["config_mismatches"][0]["field"], "tolerance");
}

#[test]
fn compare_diff_reports_requires_matching_shard_config_by_default() {
  let tmp = tempfile::TempDir::new().expect("tempdir");
  let baseline_path = tmp.path().join("baseline.json");
  let new_path = tmp.path().join("new.json");
  let out_json_fail = tmp.path().join("delta_fail.json");
  let out_html_fail = tmp.path().join("delta_fail.html");
  let out_json_ok = tmp.path().join("delta_ok.json");
  let out_html_ok = tmp.path().join("delta_ok.html");

  let mut baseline = basic_report(vec![]);
  baseline["shard"] = json!({ "index": 0, "total": 4, "discovered": 0 });
  let mut new_report = basic_report(vec![]);
  new_report["shard"] = json!({ "index": 1, "total": 4, "discovered": 0 });

  write_json(&baseline_path, &baseline);
  write_json(&new_path, &new_report);

  let output = compare_cmd(tmp.path())
    .args([
      "--baseline",
      baseline_path.to_str().unwrap(),
      "--new",
      new_path.to_str().unwrap(),
      "--json",
      out_json_fail.to_str().unwrap(),
      "--html",
      out_html_fail.to_str().unwrap(),
    ])
    .output()
    .expect("run compare_diff_reports");

  assert!(
    !output.status.success(),
    "expected non-zero exit for config mismatch\nstdout:\n{}\nstderr:\n{}",
    output_text(&output.stdout),
    output_text(&output.stderr),
  );

  let report: Value =
    serde_json::from_str(&fs::read_to_string(&out_json_fail).expect("read delta json")).unwrap();
  assert_eq!(report["config_mismatches"].as_array().unwrap().len(), 1);
  assert_eq!(report["config_mismatches"][0]["field"], "shard");

  let output = compare_cmd(tmp.path())
    .args([
      "--baseline",
      baseline_path.to_str().unwrap(),
      "--new",
      new_path.to_str().unwrap(),
      "--json",
      out_json_ok.to_str().unwrap(),
      "--html",
      out_html_ok.to_str().unwrap(),
      "--allow-config-mismatch",
    ])
    .output()
    .expect("run compare_diff_reports");

  assert!(
    output.status.success(),
    "expected success when mismatch allowed\nstdout:\n{}\nstderr:\n{}",
    output_text(&output.stdout),
    output_text(&output.stderr)
  );

  let report: Value = serde_json::from_str(&fs::read_to_string(&out_json_ok).unwrap()).unwrap();
  assert_eq!(report["config_mismatches"].as_array().unwrap().len(), 1);
  assert_eq!(report["config_mismatches"][0]["field"], "shard");
}

#[test]
fn compare_diff_reports_can_override_report_html_paths() {
  let tmp = tempfile::TempDir::new().expect("tempdir");
  let baseline_path = tmp.path().join("baseline.json");
  let new_path = tmp.path().join("new.json");
  let baseline_html = tmp.path().join("baseline_custom.html");
  let new_html = tmp.path().join("new_custom.html");
  let out_json = tmp.path().join("delta.json");
  let out_html = tmp.path().join("delta.html");

  fs::write(&baseline_html, "<!doctype html><title>baseline</title>").unwrap();
  fs::write(&new_html, "<!doctype html><title>new</title>").unwrap();

  write_json(&baseline_path, &basic_report(vec![]));
  write_json(&new_path, &basic_report(vec![]));

  let output = compare_cmd(tmp.path())
    .args([
      "--baseline",
      baseline_path.to_str().unwrap(),
      "--new",
      new_path.to_str().unwrap(),
      "--baseline-html",
      baseline_html.to_str().unwrap(),
      "--new-html",
      new_html.to_str().unwrap(),
      "--json",
      out_json.to_str().unwrap(),
      "--html",
      out_html.to_str().unwrap(),
    ])
    .output()
    .expect("run compare_diff_reports");

  assert!(
    output.status.success(),
    "expected success, got {:?}\nstdout:\n{}\nstderr:\n{}",
    output.status.code(),
    output_text(&output.stdout),
    output_text(&output.stderr),
  );

  let report: Value = serde_json::from_str(&fs::read_to_string(&out_json).unwrap()).unwrap();
  assert_eq!(
    report["baseline"]["report_html"],
    fs::canonicalize(&baseline_html).unwrap().display().to_string()
  );
  assert_eq!(
    report["new"]["report_html"],
    fs::canonicalize(&new_html).unwrap().display().to_string()
  );

  let html = fs::read_to_string(&out_html).expect("read delta html");
  assert!(html.contains("baseline_custom.html"), "missing baseline link:\n{html}");
  assert!(html.contains("new_custom.html"), "missing new link:\n{html}");
}

#[test]
fn compare_diff_reports_accepts_reports_without_ignore_alpha_field() {
  let tmp = tempfile::TempDir::new().expect("tempdir");
  let baseline_path = tmp.path().join("baseline.json");
  let new_path = tmp.path().join("new.json");
  let out_json = tmp.path().join("delta.json");
  let out_html = tmp.path().join("delta.html");

  let mut baseline = basic_report(vec![]);
  baseline
    .as_object_mut()
    .expect("baseline object")
    .remove("ignore_alpha");
  let mut new_report = basic_report(vec![]);
  new_report
    .as_object_mut()
    .expect("new report object")
    .remove("ignore_alpha");

  write_json(&baseline_path, &baseline);
  write_json(&new_path, &new_report);

  let output = compare_cmd(tmp.path())
    .args([
      "--baseline",
      baseline_path.to_str().unwrap(),
      "--new",
      new_path.to_str().unwrap(),
      "--json",
      out_json.to_str().unwrap(),
      "--html",
      out_html.to_str().unwrap(),
    ])
    .output()
    .expect("run compare_diff_reports");

  assert!(
    output.status.success(),
    "expected success, got {:?}\nstdout:\n{}\nstderr:\n{}",
    output.status.code(),
    output_text(&output.stdout),
    output_text(&output.stderr),
  );

  let report: Value = serde_json::from_str(&fs::read_to_string(&out_json).unwrap()).unwrap();
  assert!(
    report["config_mismatches"].as_array().unwrap().is_empty(),
    "ignore_alpha should default to false when missing: {report}"
  );
}

#[test]
fn compare_diff_reports_sorts_results_by_severity() {
  let tmp = tempfile::TempDir::new().expect("tempdir");
  let baseline_path = tmp.path().join("baseline.json");
  let new_path = tmp.path().join("new.json");
  let out_json = tmp.path().join("delta.json");
  let out_html = tmp.path().join("delta.html");

  let baseline = basic_report(vec![
    json!({
      "name": "regress",
      "status": "diff",
      "metrics": {
        "pixel_diff": 0,
        "total_pixels": 100,
        "diff_percentage": 0.0,
        "perceptual_distance": 0.0
      }
    }),
    json!({
      "name": "missing_new",
      "status": "diff",
      "metrics": {
        "pixel_diff": 1,
        "total_pixels": 100,
        "diff_percentage": 1.0,
        "perceptual_distance": 0.1
      }
    }),
    json!({
      "name": "improve",
      "status": "diff",
      "metrics": {
        "pixel_diff": 10,
        "total_pixels": 100,
        "diff_percentage": 10.0,
        "perceptual_distance": 0.5
      }
    }),
    json!({
      "name": "unchanged",
      "status": "diff",
      "metrics": {
        "pixel_diff": 5,
        "total_pixels": 100,
        "diff_percentage": 5.0,
        "perceptual_distance": 0.2
      }
    }),
  ]);

  let new_report = basic_report(vec![
    json!({
      "name": "regress",
      "status": "diff",
      "metrics": {
        "pixel_diff": 2,
        "total_pixels": 100,
        "diff_percentage": 2.0,
        "perceptual_distance": 0.2
      }
    }),
    json!({
      "name": "improve",
      "status": "diff",
      "metrics": {
        "pixel_diff": 2,
        "total_pixels": 100,
        "diff_percentage": 2.0,
        "perceptual_distance": 0.2
      }
    }),
    json!({
      "name": "unchanged",
      "status": "diff",
      "metrics": {
        "pixel_diff": 5,
        "total_pixels": 100,
        "diff_percentage": 5.0,
        "perceptual_distance": 0.2
      }
    }),
    json!({
      "name": "missing_base",
      "status": "diff",
      "metrics": {
        "pixel_diff": 3,
        "total_pixels": 100,
        "diff_percentage": 3.0,
        "perceptual_distance": 0.3
      }
    }),
  ]);

  write_json(&baseline_path, &baseline);
  write_json(&new_path, &new_report);

  let output = compare_cmd(tmp.path())
    .args([
      "--baseline",
      baseline_path.to_str().unwrap(),
      "--new",
      new_path.to_str().unwrap(),
      "--json",
      out_json.to_str().unwrap(),
      "--html",
      out_html.to_str().unwrap(),
    ])
    .output()
    .expect("run compare_diff_reports");

  assert!(
    output.status.success(),
    "expected success, got {:?}\nstdout:\n{}\nstderr:\n{}",
    output.status.code(),
    output_text(&output.stdout),
    output_text(&output.stderr),
  );

  let report: Value = serde_json::from_str(&fs::read_to_string(&out_json).unwrap()).unwrap();
  let results = report["results"].as_array().expect("results array");
  let names: Vec<String> = results
    .iter()
    .map(|entry| entry["name"].as_str().unwrap().to_string())
    .collect();
  assert_eq!(
    names,
    vec![
      "missing_new".to_string(),
      "regress".to_string(),
      "missing_base".to_string(),
      "improve".to_string(),
      "unchanged".to_string()
    ]
  );
}

#[test]
fn compare_diff_reports_can_filter_entries_by_name() {
  let tmp = tempfile::TempDir::new().expect("tempdir");
  let baseline_path = tmp.path().join("baseline.json");
  let new_path = tmp.path().join("new.json");
  let out_json = tmp.path().join("delta.json");
  let out_html = tmp.path().join("delta.html");

  let baseline = basic_report(vec![
    json!({
      "name": "keep",
      "status": "diff",
      "metrics": {
        "pixel_diff": 1,
        "total_pixels": 100,
        "diff_percentage": 1.0,
        "perceptual_distance": 0.1
      }
    }),
    json!({
      "name": "drop",
      "status": "diff",
      "metrics": {
        "pixel_diff": 2,
        "total_pixels": 100,
        "diff_percentage": 2.0,
        "perceptual_distance": 0.2
      }
    }),
  ]);
  let new_report = basic_report(vec![
    json!({
      "name": "keep",
      "status": "diff",
      "metrics": {
        "pixel_diff": 1,
        "total_pixels": 100,
        "diff_percentage": 1.0,
        "perceptual_distance": 0.1
      }
    }),
    json!({
      "name": "drop",
      "status": "diff",
      "metrics": {
        "pixel_diff": 2,
        "total_pixels": 100,
        "diff_percentage": 2.0,
        "perceptual_distance": 0.2
      }
    }),
  ]);

  write_json(&baseline_path, &baseline);
  write_json(&new_path, &new_report);

  let output = compare_cmd(tmp.path())
    .args([
      "--baseline",
      baseline_path.to_str().unwrap(),
      "--new",
      new_path.to_str().unwrap(),
      "--include",
      "^keep$",
      "--exclude",
      "drop",
      "--json",
      out_json.to_str().unwrap(),
      "--html",
      out_html.to_str().unwrap(),
    ])
    .output()
    .expect("run compare_diff_reports");

  assert!(
    output.status.success(),
    "expected success, got {:?}\nstdout:\n{}\nstderr:\n{}",
    output.status.code(),
    output_text(&output.stdout),
    output_text(&output.stderr),
  );

  let stdout = output_text(&output.stdout);
  assert!(stdout.contains("Filters:"), "missing Filters line:\n{stdout}");
  assert!(stdout.contains("^keep$"), "missing include filter in stdout:\n{stdout}");
  assert!(stdout.contains("drop"), "missing exclude filter in stdout:\n{stdout}");
  assert!(stdout.contains("matched=1/2"), "missing filter match count:\n{stdout}");

  let report: Value = serde_json::from_str(&fs::read_to_string(&out_json).unwrap()).unwrap();
  assert_eq!(report["totals"]["entries"], 1);
  assert_eq!(report["aggregate"]["paired_with_metrics"], 1);
  assert_eq!(report["filters"]["include"][0], "^keep$");
  assert_eq!(report["filters"]["exclude"][0], "drop");
  assert_eq!(report["filters"]["matched_entries"].as_u64(), Some(1));
  assert_eq!(report["filters"]["total_entries"].as_u64(), Some(2));

  let html = fs::read_to_string(&out_html).expect("read delta html");
  assert!(
    html.contains("<strong>Filters:</strong>"),
    "missing Filters row:\n{html}"
  );
  assert!(html.contains("<code>^keep$</code>"), "missing include filter:\n{html}");
  assert!(html.contains("<code>drop</code>"), "missing exclude filter:\n{html}");
  assert!(html.contains("matched=1/2"), "missing filter match count:\n{html}");

  let results = report["results"].as_array().expect("results array");
  assert_eq!(results.len(), 1);
  assert_eq!(results[0]["name"], "keep");
}
