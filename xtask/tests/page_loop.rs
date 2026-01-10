use std::fs;
use std::path::PathBuf;
use std::process::Command;

use tempfile::tempdir;

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask crate should live under the workspace root")
    .to_path_buf()
}

fn write_progress_file(dir: &std::path::Path, stem: &str, json: &str) {
  fs::create_dir_all(dir).expect("create progress dir");
  fs::write(dir.join(format!("{stem}.json")), json).expect("write progress json");
}

#[test]
fn dry_run_prints_expected_plan() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args(["page-loop", "--fixture", "example.com", "--dry-run"])
    .output()
    .expect("run cargo xtask page-loop --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop dry-run to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("page-loop plan:"),
    "expected plan header; got:\n{stdout}"
  );
  assert!(
    stdout.contains("fixture: example.com"),
    "expected fixture name in plan; got:\n{stdout}"
  );
  assert!(
    stdout.contains("scripts/cargo_agent.sh run --release --bin render_fixtures"),
    "expected render_fixtures command to be present; got:\n{stdout}"
  );
  assert!(
    stdout.contains("target/page_loop") && stdout.contains("example.com") && stdout.contains("fastrender"),
    "expected output path to mention target/page_loop/<fixture>/fastrender; got:\n{stdout}"
  );
}

#[test]
fn dry_run_with_chrome_enables_chrome_patching_and_diff_steps() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args(["page-loop", "--fixture", "example.com", "--chrome", "--dry-run"])
    .output()
    .expect("run cargo xtask page-loop --chrome --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop dry-run to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--patch-html-for-chrome-baseline"),
    "expected render_fixtures to be patched for chrome baselines; got:\n{stdout}"
  );
  assert!(
    stdout.contains("chrome-baseline-fixtures"),
    "expected chrome-baseline-fixtures command in plan; got:\n{stdout}"
  );
  assert!(
    stdout.contains("--bin diff_renders") || stdout.contains("diff_renders"),
    "expected diff_renders commands in plan; got:\n{stdout}"
  );
}

#[test]
fn dry_run_forwards_timeout_to_all_steps() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "page-loop",
      "--fixture",
      "example.com",
      "--chrome",
      "--timeout",
      "42",
      "--dry-run",
    ])
    .output()
    .expect("run cargo xtask page-loop --timeout 42 --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop dry-run to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  let occurrences = stdout.matches("--timeout 42").count();
  assert!(
    occurrences >= 2,
    "expected timeout to be forwarded to FastRender + Chrome commands; got:\n{stdout}"
  );
}

#[test]
fn dry_run_accepts_pageset_url() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args(["page-loop", "--pageset", "https://example.com", "--dry-run"])
    .output()
    .expect("run cargo xtask page-loop --pageset --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop --pageset dry-run to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("fixture: example.com"),
    "expected pageset URL to resolve to fixture stem; got:\n{stdout}"
  );
}

#[test]
fn from_progress_top_worst_accuracy_prefers_existing_fixtures_and_tiebreaks_perceptual() {
  let temp = tempdir().expect("tempdir");
  let progress_dir = temp.path().join("progress/pages");

  // Both pages tie on diff_percent, so selection should fall back to perceptual distance.
  // Include a "better" page that is missing a fixture to ensure fixture availability is preferred.
  write_progress_file(
    &progress_dir,
    "example.com",
    r#"{"status":"ok","accuracy":{"diff_percent":10.0,"perceptual":0.1}}"#,
  );
  write_progress_file(
    &progress_dir,
    "amazon.com",
    r#"{"status":"ok","accuracy":{"diff_percent":10.0,"perceptual":0.2}}"#,
  );
  write_progress_file(
    &progress_dir,
    "missing-fixture.test",
    r#"{"status":"ok","accuracy":{"diff_percent":99.0,"perceptual":1.0}}"#,
  );

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "page-loop",
      "--from-progress",
      progress_dir.to_string_lossy().as_ref(),
      "--top-worst-accuracy",
      "1",
      "--dry-run",
    ])
    .output()
    .expect("run cargo xtask page-loop --from-progress --top-worst-accuracy 1 --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop progress selection to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("fixture: amazon.com"),
    "expected amazon.com to be selected (perceptual tiebreak, and fixture preference over missing-fixture.test); got:\n{stdout}"
  );
}

#[test]
fn from_progress_top_slowest_selects_highest_total_ms() {
  let temp = tempdir().expect("tempdir");
  let progress_dir = temp.path().join("progress/pages");

  write_progress_file(
    &progress_dir,
    "example.com",
    r#"{"status":"ok","total_ms":10.0}"#,
  );
  write_progress_file(
    &progress_dir,
    "amazon.com",
    r#"{"status":"ok","total_ms":50.0}"#,
  );

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "page-loop",
      "--from-progress",
      progress_dir.to_string_lossy().as_ref(),
      "--top-slowest",
      "1",
      "--dry-run",
    ])
    .output()
    .expect("run cargo xtask page-loop --from-progress --top-slowest 1 --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop progress selection to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("fixture: amazon.com"),
    "expected amazon.com to be selected as the slowest page; got:\n{stdout}"
  );
}

#[test]
fn from_progress_only_failures_selects_first_failing_stem() {
  let temp = tempdir().expect("tempdir");
  let progress_dir = temp.path().join("progress/pages");

  write_progress_file(
    &progress_dir,
    "example.com",
    r#"{"status":"timeout"}"#,
  );
  write_progress_file(
    &progress_dir,
    "amazon.com",
    r#"{"status":"error"}"#,
  );

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "page-loop",
      "--from-progress",
      progress_dir.to_string_lossy().as_ref(),
      "--only-failures",
      "--dry-run",
    ])
    .output()
    .expect("run cargo xtask page-loop --from-progress --only-failures --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop progress selection to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("fixture: amazon.com"),
    "expected amazon.com to be selected as the first failing stem; got:\n{stdout}"
  );
}

#[test]
fn from_progress_hotspot_filters_candidates() {
  let temp = tempdir().expect("tempdir");
  let progress_dir = temp.path().join("progress/pages");

  write_progress_file(
    &progress_dir,
    "amazon.com",
    r#"{"status":"ok","hotspot":"css","accuracy":{"diff_percent":5.0,"perceptual":0.1}}"#,
  );
  write_progress_file(
    &progress_dir,
    "example.com",
    r#"{"status":"ok","hotspot":"layout","accuracy":{"diff_percent":99.0,"perceptual":1.0}}"#,
  );

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "page-loop",
      "--from-progress",
      progress_dir.to_string_lossy().as_ref(),
      "--top-worst-accuracy",
      "1",
      "--hotspot",
      "CSS",
      "--dry-run",
    ])
    .output()
    .expect("run cargo xtask page-loop --from-progress --hotspot CSS --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop hotspot selection to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("fixture: amazon.com"),
    "expected hotspot filter to exclude example.com and select amazon.com; got:\n{stdout}"
  );
}

#[test]
fn from_progress_defaults_to_top_worst_accuracy_1() {
  let temp = tempdir().expect("tempdir");
  let progress_dir = temp.path().join("progress/pages");

  write_progress_file(
    &progress_dir,
    "example.com",
    r#"{"status":"ok","accuracy":{"diff_percent":1.0,"perceptual":0.0}}"#,
  );
  write_progress_file(
    &progress_dir,
    "amazon.com",
    r#"{"status":"ok","accuracy":{"diff_percent":2.0,"perceptual":0.0}}"#,
  );

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "page-loop",
      "--from-progress",
      progress_dir.to_string_lossy().as_ref(),
      "--dry-run",
    ])
    .output()
    .expect("run cargo xtask page-loop --from-progress --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop progress selection to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("fixture: amazon.com"),
    "expected default selection to pick the worst-accuracy ok page (amazon.com); got:\n{stdout}"
  );
}

#[test]
fn from_progress_errors_when_no_offline_fixture_exists() {
  let temp = tempdir().expect("tempdir");
  let progress_dir = temp.path().join("progress/pages");

  write_progress_file(&progress_dir, "zzz_page_loop_missing_fixture_a", r#"{"status":"timeout"}"#);
  write_progress_file(&progress_dir, "zzz_page_loop_missing_fixture_b", r#"{"status":"error"}"#);

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "page-loop",
      "--from-progress",
      progress_dir.to_string_lossy().as_ref(),
      "--only-failures",
    ])
    .output()
    .expect("run cargo xtask page-loop --from-progress --only-failures");

  assert!(
    !output.status.success(),
    "expected page-loop to fail when the selected progress page has no offline fixture.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("zzz_page_loop_missing_fixture_a") && stderr.contains("does not have an offline fixture"),
    "expected missing-fixture error to mention the selected stem; got:\n{stderr}"
  );
  assert!(
    stderr.contains("import-page-fixture") || stderr.contains("recapture-page-fixtures"),
    "expected missing-fixture error to include a fixture capture hint; got:\n{stderr}"
  );
}
