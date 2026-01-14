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

#[test]
fn browser_headless_smoke_load_failures_are_logged_and_fall_back_to_empty_stores() {
  let _lock = super::stage_listener_test_lock();

  let dir = tempfile::tempdir().expect("temp dir");
  let session_path = dir.path().join("session.json");

  // Force profile persistence load failures by pointing the env override paths at directories
  // instead of files (reading a directory as a file should error reliably on all platforms).
  let bookmarks_path = dir.path().join("bookmarks_dir");
  std::fs::create_dir_all(&bookmarks_path).expect("create bookmarks dir");
  let history_path = dir.path().join("history_dir");
  std::fs::create_dir_all(&history_path).expect("create history dir");

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

  assert!(
    stderr.contains(&format!(
      "failed to load bookmarks from {}",
      bookmarks_path.display()
    )),
    "expected stderr to mention bookmarks load failure, got:\n{stderr}"
  );
  assert!(
    stderr.contains(&format!(
      "failed to load history from {}",
      history_path.display()
    )),
    "expected stderr to mention history load failure, got:\n{stderr}"
  );

  assert!(
    stdout
      .lines()
      .any(|line| line.starts_with("HEADLESS_BOOKMARKS source=empty ")),
    "expected headless smoke to fall back to empty bookmarks, got stdout:\n{stdout}\nstderr:\n{stderr}"
  );
  assert!(
    stdout
      .lines()
      .any(|line| line.starts_with("HEADLESS_HISTORY source=empty ")),
    "expected headless smoke to fall back to empty history, got stdout:\n{stdout}\nstderr:\n{stderr}"
  );
}

