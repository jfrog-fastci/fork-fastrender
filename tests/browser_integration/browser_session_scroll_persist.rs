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

#[test]
fn browser_persists_scroll_css_into_session_file_across_runs() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let dir = tempfile::tempdir().expect("temp dir");
  let session_path = dir.path().join("session.json");

  // First run: seed a session starting on the scrollable `about:test-scroll` fixture and request
  // that the headless smoke harness scroll the page and persist the observed scroll offset into the
  // on-disk session file.
  let seed_json = r#"{
    "version": 2,
    "windows": [{
      "tabs": [{"url": "about:test-scroll"}],
      "active_tab_index": 0
    }],
    "active_window_index": 0
  }"#;

  let (status, stderr, stdout) = run_browser_headless_smoke(
    &[],
    &session_path,
    &[
      ("FASTR_TEST_BROWSER_HEADLESS_SMOKE_SESSION_JSON", seed_json),
      ("FASTR_TEST_BROWSER_HEADLESS_SMOKE_SCROLL_TO_Y", "240"),
    ],
  );
  assert_browser_succeeded(status, &stderr, &stdout);
  assert!(
    session_path.exists(),
    "expected session file to be written at {}",
    session_path.display()
  );

  let persisted = std::fs::read_to_string(&session_path).expect("read persisted session");
  let persisted_session: fastrender::ui::BrowserSession =
    serde_json::from_str(&persisted).expect("parse persisted session JSON");
  let persisted_scroll_css = persisted_session.windows[0].tabs[0]
    .scroll_css
    .expect("expected persisted scroll_css to be present after scrolling");
  assert!(
    persisted_scroll_css.1 > 0.0,
    "expected persisted scroll_y to be non-zero, got {persisted_scroll_css:?}\nfull session JSON:\n{persisted}"
  );

  // Second run: restore from the on-disk session file and ensure the restored session includes the
  // non-zero scroll offset.
  let (status, stderr, stdout) = run_browser_headless_smoke(&[], &session_path, &[]);
  assert_browser_succeeded(status, &stderr, &stdout);

  let (source, restored_session) = parse_headless_session(&stdout);
  assert_eq!(source, "restored");
  let restored_scroll_css = restored_session.windows[0].tabs[0]
    .scroll_css
    .expect("expected restored scroll_css to be present");
  assert!(
    restored_scroll_css.1 > 0.0,
    "expected restored scroll_y to be non-zero, got {restored_scroll_css:?}\nrestored session:\n{restored_session:#?}"
  );
}
