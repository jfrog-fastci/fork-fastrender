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
fn browser_persists_and_restores_session_tabs_and_active_tab_across_runs() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let dir = tempfile::tempdir().expect("temp dir");
  let session_path = dir.path().join("session.json");

  // Seed the session via the headless override hook, including a non-default appearance payload to
  // ensure appearance settings are persisted/restored alongside tabs.
  let expected_json = r##"{
    "version": 2,
    "windows": [{
      "tabs": [
        {"url": "about:newtab", "zoom": 1.5, "pinned": true},
        {"url": "about:blank", "zoom": 0.75},
        {"url": "about:test-scroll", "zoom": 2.0, "group": 0},
        {"url": "about:error", "group": 0}
      ],
      "tab_groups": [
        {"title": "My Group", "color": "purple", "collapsed": true}
      ],
      "active_tab_index": 1,
      "show_menu_bar": false,
      "window_state": {
        "x": 123,
        "y": 456,
        "width": 1111,
        "height": 777,
        "maximized": true
      }
    }],
    "active_window_index": 0,
    "appearance": {
      "theme": "dark",
      "accent_color": "#123456",
      "high_contrast": true,
      "reduced_motion": true,
      "ui_scale": 1.25
    }
  }"##;
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
  let windows = persisted_value
    .get("windows")
    .and_then(|v| v.as_array())
    .expect("expected windows array");
  let win0 = windows.first().expect("expected one window");
  let tabs = win0
    .get("tabs")
    .and_then(|v| v.as_array())
    .expect("expected tabs array");
  assert!(
    tabs
      .first()
      .and_then(|v| v.get("pinned"))
      .and_then(|v| v.as_bool())
      == Some(true),
    "expected first tab to be pinned, got: {tabs:?}"
  );
  // `pinned=false` and `group=null` should be omitted for cleanliness/backwards compatibility.
  assert!(
    tabs.get(1).is_some_and(|v| v.get("pinned").is_none()),
    "expected pinned=false to be omitted for second tab, got: {tabs:?}"
  );
  assert!(
    tabs.get(1).is_some_and(|v| v.get("group").is_none()),
    "expected missing group for ungrouped tab, got: {tabs:?}"
  );
  let groups = win0
    .get("tab_groups")
    .and_then(|v| v.as_array())
    .expect("expected tab_groups array");
  assert_eq!(groups.len(), 1);
  assert_eq!(
    groups[0].get("title").and_then(|v| v.as_str()),
    Some("My Group")
  );
  assert_eq!(
    groups[0].get("color").and_then(|v| v.as_str()),
    Some("purple")
  );
  assert_eq!(
    groups[0].get("collapsed").and_then(|v| v.as_bool()),
    Some(true)
  );
  let appearance_value = persisted_value
    .get("appearance")
    .expect("expected persisted appearance settings");
  assert_eq!(
    appearance_value.get("theme").and_then(|v| v.as_str()),
    Some("dark")
  );
  assert_eq!(
    appearance_value
      .get("accent_color")
      .and_then(|v| v.as_str()),
    Some("#123456")
  );
  assert_eq!(
    appearance_value
      .get("high_contrast")
      .and_then(|v| v.as_bool()),
    Some(true)
  );
  assert_eq!(
    appearance_value
      .get("reduced_motion")
      .and_then(|v| v.as_bool()),
    Some(true)
  );
  assert_eq!(
    appearance_value.get("ui_scale").and_then(|v| v.as_f64()),
    Some(1.25)
  );
  assert_eq!(
    win0.get("show_menu_bar").and_then(|v| v.as_bool()),
    Some(false)
  );
  let window_state = win0
    .get("window_state")
    .and_then(|v| v.as_object())
    .expect("expected window_state object");
  assert_eq!(window_state.get("x").and_then(|v| v.as_i64()), Some(123));
  assert_eq!(window_state.get("y").and_then(|v| v.as_i64()), Some(456));
  assert_eq!(
    window_state.get("width").and_then(|v| v.as_i64()),
    Some(1111)
  );
  assert_eq!(
    window_state.get("height").and_then(|v| v.as_i64()),
    Some(777)
  );
  assert_eq!(
    window_state.get("maximized").and_then(|v| v.as_bool()),
    Some(true)
  );
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
  assert_eq!(session.appearance, expected_session.appearance);
  assert_eq!(
    session.windows[0].window_state,
    expected_session.windows[0].window_state
  );
  assert_eq!(
    session.windows[0].show_menu_bar,
    expected_session.windows[0].show_menu_bar
  );
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
  assert_eq!(session.appearance, expected_session.appearance);
  assert_eq!(
    session.windows[0].window_state,
    expected_session.windows[0].window_state
  );
  assert_eq!(
    session.windows[0].show_menu_bar,
    expected_session.windows[0].show_menu_bar
  );
}

#[test]
fn browser_restores_legacy_v1_session_file_and_upgrades_to_v2() {
  let _lock = super::stage_listener_test_lock();

  let dir = tempfile::tempdir().expect("temp dir");
  let session_path = dir.path().join("session.json");

  // Legacy v1 session format (pre multi-window):
  //   { "tabs": [...], "active_tab_index": ... }
  let legacy_v1 = serde_json::json!({
    "tabs": [
      { "url": "about:blank", "zoom": 1.25 },
      { "url": "about:error", "zoom": 0.8 },
    ],
    "active_tab_index": 1,
  });
  std::fs::write(
    &session_path,
    serde_json::to_vec(&legacy_v1).expect("serialize v1"),
  )
  .expect("write legacy session");

  // Run once with no args so restore is attempted.
  let (status, stderr, stdout) = run_browser_headless_smoke(&[], &session_path, &[]);
  assert_browser_succeeded(status, &stderr, &stdout);

  let (source, restored) = parse_headless_session(&stdout);
  assert_eq!(source, "restored");
  assert_eq!(restored.version, 2);
  assert_eq!(restored.active_window_index, 0);
  assert_eq!(restored.windows.len(), 1);
  assert_eq!(restored.windows[0].active_tab_index, 1);
  assert_eq!(restored.windows[0].tabs.len(), 2);
  assert_eq!(restored.windows[0].tabs[0].url, "about:blank");
  assert_eq!(restored.windows[0].tabs[0].zoom, Some(1.25));
  assert_eq!(restored.windows[0].tabs[1].url, "about:error");
  assert_eq!(restored.windows[0].tabs[1].zoom, Some(0.8));
  assert_eq!(
    restored.appearance,
    fastrender::ui::appearance::AppearanceSettings::default()
  );

  // The browser should rewrite the session file in the new v2 format.
  let disk_json = std::fs::read_to_string(&session_path).expect("read upgraded session file");
  let disk_session: fastrender::ui::BrowserSession =
    serde_json::from_str(&disk_json).expect("parse upgraded session JSON");
  assert_eq!(disk_session.version, 2);
  assert_eq!(disk_session, restored);
}

#[test]
fn browser_skips_restore_after_repeated_unclean_exits_and_starts_safe_session() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  const THRESHOLD: u32 = 3;

  let dir = tempfile::tempdir().expect("temp dir");
  let session_path = dir.path().join("session.json");

  let mut session = fastrender::ui::BrowserSession::single("about:blank".to_string());
  session.did_exit_cleanly = false;
  session.unclean_exit_streak = THRESHOLD;
  fastrender::ui::session::save_session_atomic(&session_path, &session).expect("seed session");

  let (status, stderr, stdout) = run_browser_headless_smoke(&[], &session_path, &[]);
  assert_browser_succeeded(status, &stderr, &stdout);

  let (source, session) = parse_headless_session(&stdout);
  assert_eq!(source, "default", "expected safe-start to avoid restoring");
  assert_eq!(session.windows.len(), 1);
  assert_eq!(session.windows[0].tabs.len(), 1);
  assert_eq!(session.windows[0].tabs[0].url, "about:newtab");

  assert!(
    stderr.contains("session restore skipped due to repeated crashes"),
    "expected crash-loop breaker message in stderr, got:\n{stderr}\nstdout:\n{stdout}"
  );
}

#[test]
fn browser_restore_flag_forces_restore_even_when_unclean_exit_streak_is_high() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  const THRESHOLD: u32 = 3;

  let dir = tempfile::tempdir().expect("temp dir");
  let session_path = dir.path().join("session.json");

  let mut session = fastrender::ui::BrowserSession::single("about:blank".to_string());
  session.did_exit_cleanly = false;
  session.unclean_exit_streak = THRESHOLD;
  fastrender::ui::session::save_session_atomic(&session_path, &session).expect("seed session");

  let (status, stderr, stdout) = run_browser_headless_smoke(&["--restore"], &session_path, &[]);
  assert_browser_succeeded(status, &stderr, &stdout);

  let (source, session) = parse_headless_session(&stdout);
  assert_eq!(source, "restored");
  assert_eq!(session.windows.len(), 1);
  assert_eq!(session.windows[0].tabs.len(), 1);
  assert_eq!(session.windows[0].tabs[0].url, "about:blank");
}
