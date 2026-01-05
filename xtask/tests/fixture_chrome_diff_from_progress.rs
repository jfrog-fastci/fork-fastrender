use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::tempdir;

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask crate should live under repo root")
    .to_path_buf()
}

fn write_fixture(fixtures_root: &Path, stem: &str) {
  let dir = fixtures_root.join(stem);
  fs::create_dir_all(&dir).expect("create fixture dir");
  fs::write(
    dir.join("index.html"),
    "<!doctype html><title>fixture</title>",
  )
  .expect("write fixture html");
}

fn write_progress_file(progress_root: &Path, stem: &str, json: &str) {
  fs::create_dir_all(progress_root).expect("create progress dir");
  fs::write(progress_root.join(format!("{stem}.json")), json).expect("write progress json");
}

#[test]
fn only_failures_errors_on_missing_fixture() {
  let temp = tempdir().expect("tempdir");
  let progress_root = temp.path().join("progress");
  let fixtures_root = temp.path().join("fixtures");
  let out_dir = temp.path().join("out");

  write_progress_file(
    &progress_root,
    "timeout.test",
    r#"{"url":"https://timeout.test/","status":"timeout"}"#,
  );
  write_progress_file(
    &progress_root,
    "ok.test",
    r#"{"url":"https://ok.test/","status":"ok"}"#,
  );
  // Only provide the OK fixture; the failing one is intentionally missing.
  write_fixture(&fixtures_root, "ok.test");

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "fixture-chrome-diff",
      "--dry-run",
      "--no-chrome",
      "--fixtures-dir",
      fixtures_root.to_string_lossy().as_ref(),
      "--from-progress",
      progress_root.to_string_lossy().as_ref(),
      "--only-failures",
      "--out-dir",
      out_dir.to_string_lossy().as_ref(),
    ])
    .output()
    .expect("run fixture-chrome-diff with --from-progress --only-failures");

  assert!(
    !output.status.success(),
    "expected command to fail on missing fixtures.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("timeout.test"),
    "error should mention the missing stem; got:\n{stderr}"
  );
  assert!(
    stderr.contains("capture-missing-failure-fixtures"),
    "error should suggest capturing/importing fixtures; got:\n{stderr}"
  );
}

#[test]
fn top_worst_accuracy_selects_and_sorts_fixtures() {
  let temp = tempdir().expect("tempdir");
  let progress_root = temp.path().join("progress");
  let fixtures_root = temp.path().join("fixtures");
  let out_dir = temp.path().join("out");

  // All pages have the same diff_percent, so selection must fall back to perceptual distance.
  write_progress_file(
    &progress_root,
    "a.test",
    r#"{"status":"ok","accuracy":{"diff_percent":10.0,"perceptual":0.1}}"#,
  );
  write_progress_file(
    &progress_root,
    "b.test",
    r#"{"status":"ok","accuracy":{"diff_percent":10.0,"perceptual":0.05}}"#,
  );
  write_progress_file(
    &progress_root,
    "c.test",
    r#"{"status":"ok","accuracy":{"diff_percent":10.0,"perceptual":0.2}}"#,
  );

  // Only fixtures for a and c exist; b should be excluded by --top-worst-accuracy.
  write_fixture(&fixtures_root, "a.test");
  write_fixture(&fixtures_root, "c.test");

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "fixture-chrome-diff",
      "--dry-run",
      "--no-chrome",
      "--fixtures-dir",
      fixtures_root.to_string_lossy().as_ref(),
      "--from-progress",
      progress_root.to_string_lossy().as_ref(),
      "--top-worst-accuracy",
      "2",
      "--out-dir",
      out_dir.to_string_lossy().as_ref(),
    ])
    .output()
    .expect("run fixture-chrome-diff with --from-progress --top-worst-accuracy");

  assert!(
    output.status.success(),
    "expected dry-run to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  let render_line = stdout
    .lines()
    .find(|line| line.contains("--bin render_fixtures"))
    .expect("render_fixtures command line should be printed");
  assert!(
    render_line.contains("--fixtures a.test,c.test"),
    "render_fixtures should receive deterministic, stem-sorted fixtures; got:\n{render_line}\nfull stdout:\n{stdout}"
  );
}

#[test]
fn from_progress_conflicts_with_all_fixtures() {
  let temp = tempdir().expect("tempdir");
  let progress_root = temp.path().join("progress");
  let fixtures_root = temp.path().join("fixtures");
  let out_dir = temp.path().join("out");

  write_progress_file(
    &progress_root,
    "timeout.test",
    r#"{"url":"https://timeout.test/","status":"timeout"}"#,
  );
  write_fixture(&fixtures_root, "timeout.test");

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "fixture-chrome-diff",
      "--dry-run",
      "--no-chrome",
      "--fixtures-dir",
      fixtures_root.to_string_lossy().as_ref(),
      "--from-progress",
      progress_root.to_string_lossy().as_ref(),
      "--only-failures",
      "--all-fixtures",
      "--out-dir",
      out_dir.to_string_lossy().as_ref(),
    ])
    .output()
    .expect("run fixture-chrome-diff with incompatible flags");

  assert!(
    !output.status.success(),
    "expected clap to reject --from-progress with --all-fixtures.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("--from-progress") && stderr.contains("--all-fixtures"),
    "error should mention the conflicting flags; got:\n{stderr}"
  );
}
