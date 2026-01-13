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

fn parse_headless_smoke_active_url(stdout: &str) -> String {
  let line = stdout
    .lines()
    .find(|line| line.starts_with("HEADLESS_SMOKE_OK "))
    .unwrap_or_else(|| panic!("expected HEADLESS_SMOKE_OK line in stdout:\n{stdout}"));
  for part in line.split_whitespace() {
    if let Some(url) = part.strip_prefix("active_url=") {
      return url.to_string();
    }
  }
  panic!("expected active_url=... in HEADLESS_SMOKE_OK line: {line:?}");
}

#[test]
fn browser_persists_and_restores_multi_window_session_active_window_and_window_state() {
  let _lock = super::stage_listener_test_lock();

  let dir = tempfile::tempdir().expect("temp dir");
  let session_path = dir.path().join("session.json");
  // Ensure the browser does not read/write global profile state outside the temp directory.
  let bookmarks_path = dir.path().join("bookmarks.json");
  let history_path = dir.path().join("history.json");

  let expected_json = r#"{
    "version": 2,
    "windows": [
      {
        "tabs": [
          {"url": "about:newtab"},
          {"url": "about:blank"},
          {"url": "about:error"}
        ],
        "active_tab_index": 2,
        "window_state": {"x": 10, "y": 20, "width": 800, "height": 600, "maximized": true}
      },
      {
        "tabs": [
          {"url": "about:newtab"},
          {"url": "about:blank"},
          {"url": "about:test-scroll"}
        ],
        "active_tab_index": 1,
        "window_state": {"x": 30, "y": 40, "width": 1024, "height": 768, "maximized": true}
      }
    ],
    "active_window_index": 1
  }"#;
  let expected_session = fastrender::ui::session::parse_session_json(expected_json)
    .expect("parse expected session JSON");
  let expected_active_url = expected_session
    .windows
    .get(expected_session.active_window_index)
    .and_then(|w| w.tabs.get(w.active_tab_index))
    .map(|t| t.url.clone())
    .unwrap_or_else(|| panic!("expected an active tab URL in expected session: {expected_session:?}"));

  // First run: seed the session via the headless override hook and ensure it gets written.
  let (status, stderr, stdout) = run_browser_headless_smoke(
    &[],
    &session_path,
    &[
      (
        "FASTR_TEST_BROWSER_HEADLESS_SMOKE_SESSION_JSON",
        expected_json,
      ),
      (
        "FASTR_BROWSER_BOOKMARKS_PATH",
        bookmarks_path.to_str().unwrap(),
      ),
      ("FASTR_BROWSER_HISTORY_PATH", history_path.to_str().unwrap()),
    ],
  );
  assert_browser_succeeded(status, &stderr, &stdout);

  let (source, session) = parse_headless_session(&stdout);
  assert_eq!(source, "override");
  assert_eq!(session, expected_session);
  assert_eq!(parse_headless_smoke_active_url(&stdout), expected_active_url);

  assert!(
    session_path.exists(),
    "expected browser to write session file at {}",
    session_path.display()
  );
  let persisted = std::fs::read_to_string(&session_path).expect("read persisted session");
  let persisted_value: serde_json::Value =
    serde_json::from_str(&persisted).expect("parse persisted session JSON");
  let expected_value: serde_json::Value =
    serde_json::from_str(expected_json).expect("parse expected session JSON as value");
  let windows = persisted_value
    .get("windows")
    .and_then(|v| v.as_array())
    .expect("expected windows array");
  assert_eq!(
    windows.len(),
    2,
    "expected persisted session to contain two windows, got: {persisted_value:?}"
  );
  assert_eq!(
    persisted_value
      .get("active_window_index")
      .and_then(|v| v.as_u64()),
    Some(1)
  );
  let expected_windows = expected_value
    .get("windows")
    .and_then(|v| v.as_array())
    .expect("expected windows array in expected session JSON");
  assert_eq!(
    expected_windows.len(),
    windows.len(),
    "expected test fixture to contain same window count as persisted session"
  );
  for (idx, (persisted_win, expected_win)) in
    windows.iter().zip(expected_windows.iter()).enumerate()
  {
    let persisted_state = persisted_win
      .get("window_state")
      .and_then(|v| v.as_object())
      .unwrap_or_else(|| {
        panic!(
          "expected window_state object for window {idx}, got: {persisted_win:?}"
        )
      });
    let expected_state = expected_win
      .get("window_state")
      .and_then(|v| v.as_object())
      .unwrap_or_else(|| {
        panic!("expected window_state object in fixture for window {idx}, got: {expected_win:?}")
      });
    assert_eq!(
      persisted_state.get("x").and_then(|v| v.as_i64()),
      expected_state.get("x").and_then(|v| v.as_i64()),
      "window_state.x mismatch for window {idx}: {persisted_state:?}"
    );
    assert_eq!(
      persisted_state.get("y").and_then(|v| v.as_i64()),
      expected_state.get("y").and_then(|v| v.as_i64()),
      "window_state.y mismatch for window {idx}: {persisted_state:?}"
    );
    assert_eq!(
      persisted_state.get("width").and_then(|v| v.as_i64()),
      expected_state.get("width").and_then(|v| v.as_i64()),
      "window_state.width mismatch for window {idx}: {persisted_state:?}"
    );
    assert_eq!(
      persisted_state.get("height").and_then(|v| v.as_i64()),
      expected_state.get("height").and_then(|v| v.as_i64()),
      "window_state.height mismatch for window {idx}: {persisted_state:?}"
    );
    assert_eq!(
      persisted_state.get("maximized").and_then(|v| v.as_bool()),
      expected_state.get("maximized").and_then(|v| v.as_bool()),
      "window_state.maximized mismatch for window {idx}: {persisted_state:?}"
    );
  }

  // Second run: no override env → restore from the on-disk session.
  let (status, stderr, stdout) = run_browser_headless_smoke(
    &[],
    &session_path,
    &[
      (
        "FASTR_BROWSER_BOOKMARKS_PATH",
        bookmarks_path.to_str().unwrap(),
      ),
      ("FASTR_BROWSER_HISTORY_PATH", history_path.to_str().unwrap()),
    ],
  );
  assert_browser_succeeded(status, &stderr, &stdout);
  let (source, session) = parse_headless_session(&stdout);
  assert_eq!(source, "restored");
  assert_eq!(session, expected_session);
  assert_eq!(
    session.windows.len(),
    expected_session.windows.len(),
    "expected restored session to contain the same number of windows as the fixture"
  );
  for (idx, (actual_win, expected_win)) in session
    .windows
    .iter()
    .zip(expected_session.windows.iter())
    .enumerate()
  {
    assert_eq!(
      actual_win.active_tab_index, expected_win.active_tab_index,
      "active_tab_index mismatch for restored window {idx}"
    );
    let actual_urls: Vec<&str> = actual_win.tabs.iter().map(|t| t.url.as_str()).collect();
    let expected_urls: Vec<&str> = expected_win.tabs.iter().map(|t| t.url.as_str()).collect();
    assert_eq!(actual_urls, expected_urls, "tab URL list mismatch for restored window {idx}");
  }
  assert_eq!(parse_headless_smoke_active_url(&stdout), expected_active_url);
}
