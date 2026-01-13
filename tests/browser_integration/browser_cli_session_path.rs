#![cfg(all(target_os = "linux", feature = "browser_ui"))]

use std::process::Command;

#[test]
fn browser_session_path_cli_overrides_env_and_creates_files() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();

  let dir = tempfile::tempdir().expect("temp dir");
  let env_session_path = dir.path().join("env_session.json");
  let cli_session_path = dir.path().join("cli_session.json");

  let cli_lock_path = cli_session_path.with_extension("lock");
  let env_lock_path = env_session_path.with_extension("lock");

  let run_limited = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/run_limited.sh");
  let output = Command::new("bash")
    .arg(run_limited)
    .args(["--as", "64G", "--"])
    .arg(env!("CARGO_BIN_EXE_browser"))
    .arg("--headless-smoke")
    .arg("--session-path")
    .arg(&cli_session_path)
    // Ensure the CLI flag takes precedence over the env var.
    .env("FASTR_BROWSER_SESSION_PATH", &env_session_path)
    // Ensure we don't accidentally bypass startup/locking logic via an inherited test knob.
    .env_remove("FASTR_TEST_BROWSER_EXIT_IMMEDIATELY")
    // Keep the smoke test cheap/deterministic even if the parent environment has a larger Rayon
    // pool configured.
    .env("RAYON_NUM_THREADS", "1")
    .output()
    .expect("spawn browser --headless-smoke");

  assert!(
    output.status.success(),
    "browser exited non-zero: {:?}\nstderr:\n{}\nstdout:\n{}",
    output.status.code(),
    String::from_utf8_lossy(&output.stderr),
    String::from_utf8_lossy(&output.stdout)
  );

  assert!(
    cli_session_path.exists(),
    "expected session file to exist at CLI path {}",
    cli_session_path.display()
  );
  assert!(
    cli_lock_path.exists(),
    "expected lock file to exist at CLI path {}",
    cli_lock_path.display()
  );

  assert!(
    !env_session_path.exists(),
    "expected env session path to be ignored (no file should be created): {}",
    env_session_path.display()
  );
  assert!(
    !env_lock_path.exists(),
    "expected env session path lock file to be ignored (no file should be created): {}",
    env_lock_path.display()
  );

  let session = fastrender::ui::session::load_session(&cli_session_path)
    .expect("read session file")
    .expect("session should exist");
  assert_eq!(session.version, 2);
}
