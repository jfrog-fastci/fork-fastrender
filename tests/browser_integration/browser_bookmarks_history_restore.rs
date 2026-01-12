#![cfg(all(target_os = "linux", feature = "browser_ui"))]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

fn run_browser_headless_smoke(
  args: &[&str],
  session_path: &Path,
  extra_env: &[(&str, &str)],
) -> (ExitStatus, String, String) {
  let run_limited = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/run_limited.sh");
  let mut cmd = Command::new("bash");
  cmd
    .arg(run_limited)
    .args(["--as", "64G", "--"])
    .arg(env!("CARGO_BIN_EXE_browser"))
    .args(args)
    .env("RAYON_NUM_THREADS", "1")
    .env("FASTR_USE_BUNDLED_FONTS", "1")
    .env("FASTR_TEST_BROWSER_HEADLESS_SMOKE", "1")
    .env("FASTR_BROWSER_SESSION_PATH", session_path);
  for (k, v) in extra_env {
    cmd.env(k, v);
  }

  let output = cmd.output().expect("spawn browser");
  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
  (output.status, stderr, stdout)
}

fn assert_browser_succeeded(status: ExitStatus, stderr: &str, stdout: &str) {
  assert!(
    status.success(),
    "browser exited non-zero: {:?}\nstderr:\n{}\nstdout:\n{}",
    status.code(),
    stderr,
    stdout
  );
  assert!(
    stdout.contains("HEADLESS_SMOKE_OK"),
    "expected headless smoke marker, got stdout:\n{stdout}\nstderr:\n{stderr}"
  );
}

fn parse_headless_json(stdout: &str, prefix: &str) -> (String, serde_json::Value) {
  let line = stdout
    .lines()
    .find(|line| line.starts_with(prefix))
    .unwrap_or_else(|| panic!("expected {prefix} line in stdout:\n{stdout}"));
  let rest = line.strip_prefix(prefix).expect("strip prefix");
  let (source_part, json) = rest
    .split_once(' ')
    .unwrap_or_else(|| panic!("unexpected {prefix} format: {line:?}"));
  let source = source_part
    .strip_prefix("source=")
    .unwrap_or_else(|| panic!("unexpected {prefix} source prefix: {line:?}"))
    .to_string();
  let value: serde_json::Value = serde_json::from_str(json).expect("parse JSON");
  (source, value)
}

#[test]
fn browser_persists_and_restores_bookmarks_and_history_across_runs() {
  let _lock = super::stage_listener_test_lock();

  let dir = tempfile::tempdir().expect("temp dir");
  let session_path = dir.path().join("session.json");
  let bookmarks_path = dir.path().join("bookmarks.json");
  let history_path = dir.path().join("history.json");

  // Legacy headless-smoke schema (pre-BH-04). The browser should migrate this into the new
  // `BookmarkStore` format.
  let seed_bookmarks: serde_json::Value =
    serde_json::from_str(r#"[{"title":"Example","url":"https://example.com"}]"#)
      .expect("parse seed bookmarks JSON");
  let expected_bookmarks: serde_json::Value = serde_json::from_str(
    r#"{"version":1,"next_id":2,"roots":[1],"nodes":{"1":{"type":"bookmark","id":1,"url":"https://example.com","title":"Example","added_at_ms":0}}}"#,
  )
  .expect("parse expected migrated bookmarks JSON");
  let expected_history: serde_json::Value =
    serde_json::from_str(r#"[{"title":"Example","url":"https://example.com","ts":123}]"#)
      .expect("parse expected history JSON");

  let seed_bookmarks_json =
    serde_json::to_string(&seed_bookmarks).expect("serialize seed bookmarks");
  let expected_history_json =
    serde_json::to_string(&expected_history).expect("serialize expected history");

  // First run: seed bookmarks/history via override env vars and ensure they're written to disk.
  let (status, stderr, stdout) = run_browser_headless_smoke(
    &[],
    &session_path,
    &[
      ("FASTR_BROWSER_BOOKMARKS_PATH", bookmarks_path.to_str().unwrap()),
      ("FASTR_BROWSER_HISTORY_PATH", history_path.to_str().unwrap()),
      (
        "FASTR_TEST_BROWSER_HEADLESS_SMOKE_BOOKMARKS_JSON",
        &seed_bookmarks_json,
      ),
      (
        "FASTR_TEST_BROWSER_HEADLESS_SMOKE_HISTORY_JSON",
        &expected_history_json,
      ),
    ],
  );
  assert_browser_succeeded(status, &stderr, &stdout);

  let (bookmarks_source, bookmarks) = parse_headless_json(&stdout, "HEADLESS_BOOKMARKS ");
  assert_eq!(bookmarks_source, "override");
  assert_eq!(bookmarks, expected_bookmarks);

  let (history_source, history) = parse_headless_json(&stdout, "HEADLESS_HISTORY ");
  assert_eq!(history_source, "override");
  assert_eq!(history, expected_history);

  assert!(
    bookmarks_path.exists(),
    "expected browser to write bookmarks file at {}",
    bookmarks_path.display()
  );
  assert!(
    history_path.exists(),
    "expected browser to write history file at {}",
    history_path.display()
  );

  let bookmarks_on_disk: serde_json::Value =
    serde_json::from_str(&std::fs::read_to_string(&bookmarks_path).expect("read bookmarks file"))
      .expect("parse bookmarks file JSON");
  assert_eq!(bookmarks_on_disk, expected_bookmarks);

  let history_on_disk: serde_json::Value =
    serde_json::from_str(&std::fs::read_to_string(&history_path).expect("read history file"))
      .expect("parse history file JSON");
  assert_eq!(history_on_disk, expected_history);

  // Second run: without overrides, expect the browser to load from disk.
  let (status, stderr, stdout) = run_browser_headless_smoke(
    &[],
    &session_path,
    &[
      ("FASTR_BROWSER_BOOKMARKS_PATH", bookmarks_path.to_str().unwrap()),
      ("FASTR_BROWSER_HISTORY_PATH", history_path.to_str().unwrap()),
    ],
  );
  assert_browser_succeeded(status, &stderr, &stdout);

  let (bookmarks_source, bookmarks) = parse_headless_json(&stdout, "HEADLESS_BOOKMARKS ");
  assert_eq!(bookmarks_source, "disk");
  assert_eq!(bookmarks, expected_bookmarks);

  let (history_source, history) = parse_headless_json(&stdout, "HEADLESS_HISTORY ");
  assert_eq!(history_source, "disk");
  assert_eq!(history, expected_history);
}
