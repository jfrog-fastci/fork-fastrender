use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn write_progress_stub(
  path: &Path,
  status: &str,
  last_good_commit: &str,
  last_regression_commit: &str,
) {
  let progress = serde_json::json!({
    "url": "https://example.test/",
    "status": status,
    "total_ms": 1.0,
    "stages_ms": {
      "fetch": 0.0,
      "css": 0.0,
      "cascade": 0.0,
      "box_tree": 0.0,
      "layout": 0.0,
      "paint": 0.0
    },
    "notes": "",
    "hotspot": "",
    "last_good_commit": last_good_commit,
    "last_regression_commit": last_regression_commit
  });
  fs::write(
    path,
    serde_json::to_string_pretty(&progress).expect("serialize progress json"),
  )
  .expect("write stub progress json");
}

fn read_progress(path: &Path) -> Value {
  let raw = fs::read_to_string(path).expect("read progress json");
  serde_json::from_str(&raw).expect("parse progress json")
}

fn progress_field(progress: &Value, key: &str) -> String {
  progress
    .get(key)
    .and_then(|v| v.as_str())
    .unwrap_or_default()
    .to_string()
}

fn ok_html() -> &'static str {
  "<!doctype html><html><body><p>ok</p></body></html>"
}

fn timeout_html() -> String {
  // Ensure the box-tree generator traverses enough nodes that it performs deadline checks while
  // StageHeartbeat is set to `box_tree` (mirrors the dedicated box-tree timeout integration test).
  let mut body = String::new();
  for _ in 0..300 {
    body.push_str("<div></div>");
  }
  format!("<!doctype html><title>Timeout</title><body>{body}</body>")
}

fn run_worker(
  temp: &TempDir,
  cache_path: &Path,
  progress_path: &Path,
  html: &str,
  stem: &str,
  git_sha: &str,
  github_sha: Option<&str>,
  soft_timeout_ms: Option<u64>,
  extra_env: &[(&str, &str)],
) -> Value {
  fs::write(cache_path, html).expect("write cache html");

  let mut cmd = Command::new(env!("CARGO_BIN_EXE_pageset_progress"));
  cmd
    .env("DISK_CACHE", "0")
    .env("NO_DISK_CACHE", "1")
    .env("FASTR_GIT_SHA", git_sha);
  if let Some(github_sha) = github_sha {
    cmd.env("GITHUB_SHA", github_sha);
  }
  for (key, val) in extra_env {
    cmd.env(key, val);
  }

  cmd
    .current_dir(temp.path())
    .args([
      "worker",
      "--cache-path",
      cache_path.to_str().unwrap(),
      "--stem",
      stem,
      "--progress-path",
      progress_path.to_str().unwrap(),
      "--viewport",
      "800x600",
      "--dpr",
      "1.0",
      "--user-agent",
      "pageset-progress-test",
      "--accept-language",
      "en-US",
      "--diagnostics",
      "none",
      "--bundled-fonts",
    ]);

  if let Some(ms) = soft_timeout_ms {
    cmd.args(["--soft-timeout-ms", &ms.to_string()]);
  }

  if extra_env.iter().any(|(k, _)| *k == "FASTR_TEST_BOX_TREE_DELAY_MS") {
    // Enable StageHeartbeatWriter so `FASTR_TEST_BOX_TREE_DELAY_MS` can take effect.
    cmd.args(["--stage-path", temp.path().join("stage.txt").to_str().unwrap()]);
  }

  let status = cmd.status().expect("run pageset_progress worker");
  assert!(
    status.success(),
    "worker should exit successfully, got {status:?}"
  );

  read_progress(progress_path)
}

#[test]
fn ok_to_timeout_records_regression_commit_once() {
  let temp = TempDir::new().expect("tempdir");
  let cache_path = temp.path().join("page.html");
  let progress_path = temp.path().join("progress.json");

  write_progress_stub(&progress_path, "ok", "good123", "");

  let progress = run_worker(
    &temp,
    &cache_path,
    &progress_path,
    &timeout_html(),
    "example",
    "reg456",
    None,
    Some(1000),
    &[("FASTR_TEST_BOX_TREE_DELAY_MS", "1200")],
  );

  assert_eq!(progress["status"], "timeout");
  assert_eq!(progress_field(&progress, "last_regression_commit"), "reg456");
  assert_eq!(progress_field(&progress, "last_good_commit"), "good123");
}

#[test]
fn timeout_to_ok_sets_last_good_commit() {
  let temp = TempDir::new().expect("tempdir");
  let cache_path = temp.path().join("page.html");
  let progress_path = temp.path().join("progress.json");

  write_progress_stub(&progress_path, "timeout", "", "reg111");

  let progress = run_worker(
    &temp,
    &cache_path,
    &progress_path,
    ok_html(),
    "example",
    "good222",
    None,
    None,
    &[],
  );

  assert_eq!(progress["status"], "ok");
  assert_eq!(progress_field(&progress, "last_good_commit"), "good222");
  assert_eq!(progress_field(&progress, "last_regression_commit"), "reg111");
}

#[test]
fn ok_to_ok_keeps_last_good_stable() {
  let temp = TempDir::new().expect("tempdir");
  let cache_path = temp.path().join("page.html");
  let progress_path = temp.path().join("progress.json");

  write_progress_stub(&progress_path, "ok", "stable_good", "");

  let progress = run_worker(
    &temp,
    &cache_path,
    &progress_path,
    ok_html(),
    "example",
    "newsha",
    None,
    None,
    &[],
  );

  assert_eq!(progress["status"], "ok");
  assert_eq!(progress_field(&progress, "last_good_commit"), "stable_good");
  assert_eq!(progress_field(&progress, "last_regression_commit"), "");
}

#[test]
fn timeout_to_timeout_keeps_commits_stable() {
  let temp = TempDir::new().expect("tempdir");
  let cache_path = temp.path().join("page.html");
  let progress_path = temp.path().join("progress.json");

  write_progress_stub(
    &progress_path,
    "timeout",
    "historic_good",
    "historic_regression",
  );

  let progress = run_worker(
    &temp,
    &cache_path,
    &progress_path,
    &timeout_html(),
    "example",
    "othersha",
    None,
    Some(1000),
    &[("FASTR_TEST_BOX_TREE_DELAY_MS", "1200")],
  );

  assert_eq!(progress["status"], "timeout");
  assert_eq!(progress_field(&progress, "last_good_commit"), "historic_good");
  assert_eq!(
    progress_field(&progress, "last_regression_commit"),
    "historic_regression"
  );
}

#[test]
fn current_git_sha_uses_env_fallback() {
  let expected = "env-fallback-sha";
  let temp = TempDir::new().expect("tempdir");
  let cache_path = temp.path().join("page.html");
  let progress_path = temp.path().join("progress.json");

  let progress = run_worker(
    &temp,
    &cache_path,
    &progress_path,
    ok_html(),
    "example",
    expected,
    Some("wrong-sha"),
    None,
    &[],
  );

  assert_eq!(progress["status"], "ok");
  assert_eq!(progress_field(&progress, "last_good_commit"), expected);
}
