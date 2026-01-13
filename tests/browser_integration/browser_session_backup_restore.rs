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
    .env("FASTR_TEST_BROWSER_HEADLESS_SMOKE", "1")
    .env("FASTR_BROWSER_SESSION_PATH", session_path)
    // Ensure we truly load from disk rather than being influenced by any inherited override env
    // from other tests or a developer environment.
    .env_remove("FASTR_TEST_BROWSER_HEADLESS_SMOKE_SESSION_JSON")
    // Avoid other headless-test modes taking precedence if inherited from the parent environment.
    .env_remove("FASTR_TEST_BROWSER_EXIT_IMMEDIATELY")
    .env_remove("FASTR_TEST_BROWSER_HEADLESS_CRASH_SMOKE")
    // Keep this test hermetic: we want the startup session to come from disk, not from unrelated
    // headless-smoke override hooks.
    .env_remove("FASTR_TEST_BROWSER_HEADLESS_SMOKE_BOOKMARKS_JSON")
    .env_remove("FASTR_TEST_BROWSER_HEADLESS_SMOKE_HISTORY_JSON")
    .env_remove("FASTR_TEST_BROWSER_HEADLESS_SMOKE_DISABLE_WORKER");
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

fn parse_headless_session(stdout: &str) -> (String, fastrender::ui::BrowserSession) {
  let line = stdout
    .lines()
    .find(|line| line.starts_with("HEADLESS_SESSION "))
    .unwrap_or_else(|| panic!("expected HEADLESS_SESSION line in stdout:\n{stdout}"));
  let rest = line
    .strip_prefix("HEADLESS_SESSION ")
    .expect("strip prefix");
  let (source_part, session_json) = rest
    .split_once(' ')
    .unwrap_or_else(|| panic!("unexpected HEADLESS_SESSION format: {line:?}"));
  let source = source_part
    .strip_prefix("source=")
    .unwrap_or_else(|| panic!("unexpected HEADLESS_SESSION source prefix: {line:?}"))
    .to_string();
  let session: fastrender::ui::BrowserSession =
    serde_json::from_str(session_json).expect("parse HEADLESS_SESSION JSON");
  (source, session)
}

#[test]
fn browser_restores_session_from_backup_when_primary_file_is_corrupted() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let dir = tempfile::tempdir().expect("temp dir");
  let session_path = dir.path().join("session.json");
  let backup_path = session_path.with_extension("json.bak"); // session.json.bak

  std::fs::write(&session_path, "{not valid json").expect("write corrupted primary session.json");

  // Ensure the backup is not the default single-tab newtab session so we can assert restoration.
  let backup_json = r#"{
    "version": 2,
    "home_url": "about:blank",
    "windows": [{
      "tabs": [
        {"url": "about:blank", "zoom": 1.25},
        {"url": "about:test-scroll", "zoom": 0.75, "pinned": true},
        {"url": "about:error", "zoom": 2.0}
      ],
      "active_tab_index": 2
    }],
    "active_window_index": 0,
    "appearance": {
      "theme": "dark",
      "high_contrast": true,
      "reduced_motion": true,
      "ui_scale": 1.25
    }
  }"#;
  std::fs::write(&backup_path, backup_json).expect("write backup session.json.bak");

  let expected_session = fastrender::ui::session::parse_session_json(backup_json)
    .expect("parse expected backup session JSON");

  let (status, stderr, stdout) = run_browser_headless_smoke(&[], &session_path, &[]);
  assert_browser_succeeded(status, &stderr, &stdout);

  assert!(
    stdout.contains("HEADLESS_SESSION source=restored "),
    "expected restored session source marker, got stdout:\n{stdout}\nstderr:\n{stderr}"
  );
  assert!(
    stderr.contains("recovered from backup"),
    "expected stderr to mention backup recovery, got stderr:\n{stderr}\nstdout:\n{stdout}"
  );
  let (source, session) = parse_headless_session(&stdout);
  assert_eq!(source, "restored");
  assert_eq!(session, expected_session);

  // Backup recovery should also rewrite the primary session file so subsequent launches don't keep
  // tripping over a corrupted session.json.
  let disk_json = std::fs::read_to_string(&session_path).expect("read rewritten session.json");
  let disk_session =
    fastrender::ui::session::parse_session_json(&disk_json).expect("parse rewritten session JSON");
  assert_eq!(disk_session, expected_session);
}
