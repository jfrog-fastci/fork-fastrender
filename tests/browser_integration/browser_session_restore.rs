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
fn browser_persists_and_restores_session_tabs_and_active_tab_across_runs() {
  let _lock = super::stage_listener_test_lock();

  let dir = tempfile::tempdir().expect("temp dir");
  let session_path = dir.path().join("session.json");

  // Provide a legacy v1 session payload and assert the browser upgrades it to the v2
  // multi-window in-memory/session-file representation.
  let expected_json = r#"{
    "tabs": [
      {"url": "about:newtab", "zoom": 1.5},
      {"url": "about:blank", "zoom": 0.75},
      {"url": "about:test-scroll", "zoom": 2.0}
    ],
    "active_tab_index": 2
  }"#;
  let expected_session = fastrender::ui::session::parse_session_json(expected_json)
    .expect("parse expected session JSON");

  // First run: seed the session via the headless override hook and ensure it gets written.
  let (status, stderr, stdout) = run_browser_headless_smoke(
    &[],
    &session_path,
    &[(
      "FASTR_TEST_BROWSER_HEADLESS_SMOKE_SESSION_JSON",
      expected_json,
    )],
  );
  assert_browser_succeeded(status, &stderr, &stdout);
  assert!(
    session_path.exists(),
    "expected browser to write session file at {}",
    session_path.display()
  );
  let (source, session) = parse_headless_session(&stdout);
  assert_eq!(source, "override");
  assert_eq!(session, expected_session);
  let persisted = std::fs::read_to_string(&session_path).expect("read persisted session");
  let persisted_value: serde_json::Value =
    serde_json::from_str(&persisted).expect("parse persisted session JSON");
  assert_eq!(
    persisted_value.get("version").and_then(|v| v.as_u64()),
    Some(2)
  );
  assert!(persisted_value.get("windows").is_some());
  // Legacy v1 top-level keys should never be written.
  assert!(persisted_value.get("tabs").is_none());
  assert!(persisted_value.get("active_tab_index").is_none());

  // Second run: no args → restore from the on-disk session.
  let (status, stderr, stdout) = run_browser_headless_smoke(&[], &session_path, &[]);
  assert_browser_succeeded(status, &stderr, &stdout);
  let (source, session) = parse_headless_session(&stdout);
  assert_eq!(source, "restored");
  assert_eq!(session, expected_session);

  // Third run: `<url>` overrides restore by default.
  let (status, stderr, stdout) = run_browser_headless_smoke(&["about:error"], &session_path, &[]);
  assert_browser_succeeded(status, &stderr, &stdout);
  let (source, session) = parse_headless_session(&stdout);
  assert_eq!(source, "cli");
  assert_eq!(session.windows.len(), 1);
  assert_eq!(session.active_window_index, 0);
  assert_eq!(session.windows[0].tabs.len(), 1);
  assert_eq!(session.windows[0].active_tab_index, 0);
  assert_eq!(session.windows[0].tabs[0].url, "about:error");
  let session_after_cli_override = session;

  // Fourth run: `<url>` + `--restore` forces restoring the prior session.
  // Use a different `<url>` so we can assert that the CLI arg is *ignored* when restoring.
  let (status, stderr, stdout) =
    run_browser_headless_smoke(&["--restore", "about:blank"], &session_path, &[]);
  assert_browser_succeeded(status, &stderr, &stdout);
  let (source, session) = parse_headless_session(&stdout);
  assert_eq!(source, "restored");
  assert_eq!(session, session_after_cli_override);

  // Fifth run: `--no-restore` disables restoring even when a session exists.
  let (status, stderr, stdout) = run_browser_headless_smoke(&["--no-restore"], &session_path, &[]);
  assert_browser_succeeded(status, &stderr, &stdout);
  let (source, session) = parse_headless_session(&stdout);
  assert_eq!(source, "default");
  assert_eq!(session.windows.len(), 1);
  assert_eq!(session.active_window_index, 0);
  assert_eq!(session.windows[0].tabs.len(), 1);
  assert_eq!(session.windows[0].active_tab_index, 0);
  assert_eq!(session.windows[0].tabs[0].url, "about:newtab");
}
