#![cfg(all(target_os = "linux", feature = "browser_ui"))]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

fn run_browser_headless_smoke(args: &[&str], session_path: &Path) -> (ExitStatus, String, String) {
  let run_limited = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/run_limited.sh");
  let mut cmd = Command::new("bash");
  cmd
    .arg(run_limited)
    .args(["--as", "64G", "--"])
    .arg(env!("CARGO_BIN_EXE_browser"))
    .args(args)
    // Keep the smoke test cheap/deterministic even if the parent environment has a larger Rayon
    // pool configured.
    .env("RAYON_NUM_THREADS", "1")
    // Keep the test headless even if CLI flags change; `browser` supports this legacy hook.
    .env("FASTR_TEST_BROWSER_HEADLESS_SMOKE", "1")
    .env("FASTR_BROWSER_SESSION_PATH", session_path)
    // Ensure the headless smoke harness doesn't touch the user's config directory.
    .env(
      "FASTR_BROWSER_BOOKMARKS_PATH",
      session_path.with_file_name("bookmarks.json"),
    )
    .env(
      "FASTR_BROWSER_HISTORY_PATH",
      session_path.with_file_name("history.json"),
    )
    // Ensure we don't accidentally bypass session restore via an inherited environment knob.
    .env_remove("FASTR_TEST_BROWSER_EXIT_IMMEDIATELY")
    // Ensure we exercise restore rather than the headless override hook.
    .env_remove("FASTR_TEST_BROWSER_HEADLESS_SMOKE_SESSION_JSON");

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
fn browser_restores_unclean_session_and_emits_warning() {
  let _lock = super::stage_listener_test_lock();

  let dir = tempfile::tempdir().expect("temp dir");
  let session_path = dir.path().join("session.json");

  // Seed an on-disk session that indicates the prior browser process crashed.
  let unclean_json = r#"{
    "version": 2,
    "windows": [{
      "tabs": [{"url": "about:blank"}],
      "active_tab_index": 0
    }],
    "active_window_index": 0,
    "did_exit_cleanly": false
  }"#;
  std::fs::write(&session_path, unclean_json).expect("write unclean session file");

  // Run once with no URL args so restore is attempted.
  let (status, stderr, stdout) = run_browser_headless_smoke(&[], &session_path);
  assert_browser_succeeded(status, &stderr, &stdout);

  let (source, restored) = parse_headless_session(&stdout);
  assert_eq!(source, "restored");
  assert!(
    !restored.did_exit_cleanly,
    "expected restored session to preserve did_exit_cleanly=false; got session: {restored:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
  );

  assert!(
    stderr.contains("previous session ended unexpectedly")
      && stderr.contains("restoring")
      && stderr.contains("unclean_exit_streak"),
    "expected unclean-session restore warning in stderr; stderr:\n{stderr}\nstdout:\n{stdout}"
  );

  // The headless smoke harness explicitly clears the crash marker before saving.
  let on_disk = std::fs::read_to_string(&session_path).expect("read rewritten session file");
  let parsed_on_disk: fastrender::ui::BrowserSession =
    serde_json::from_str(&on_disk).expect("parse rewritten session JSON");
  assert!(
    parsed_on_disk.did_exit_cleanly,
    "expected headless smoke harness to rewrite did_exit_cleanly=true; got session: {parsed_on_disk:?}\nfile:\n{on_disk}"
  );
  assert_eq!(
    parsed_on_disk.unclean_exit_streak, 0,
    "expected headless smoke harness to reset unclean_exit_streak=0 on clean exit; got session: {parsed_on_disk:?}\nfile:\n{on_disk}"
  );
}
