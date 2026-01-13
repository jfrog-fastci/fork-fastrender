#![cfg(all(target_os = "linux", feature = "browser_ui"))]

use std::process::Command;

#[test]
fn browser_refuses_to_start_when_cli_session_path_is_locked() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();

  let dir = tempfile::tempdir().expect("temp dir");
  let env_session_path = dir.path().join("env_session.json");
  let cli_session_path = dir.path().join("cli_session.json");

  let _lock = fastrender::ui::session::acquire_session_lock(&cli_session_path)
    .expect("acquire session lock in test process");

  let run_limited = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/run_limited.sh");
  let output = Command::new("bash")
    .arg(run_limited)
    .args(["--as", "64G", "--"])
    .arg(env!("CARGO_BIN_EXE_browser"))
    .arg("--headless-smoke")
    .arg("--session-path")
    .arg(&cli_session_path)
    // Ensure the CLI flag is the value used for locking (even if the env var is set).
    .env("FASTR_BROWSER_SESSION_PATH", &env_session_path)
    // Keep the test headless even if CLI flags change; `browser` supports this legacy hook.
    .env("FASTR_TEST_BROWSER_HEADLESS_SMOKE", "1")
    // Ensure we don't accidentally bypass session locking via an inherited CI/test environment knob.
    .env_remove("FASTR_TEST_BROWSER_EXIT_IMMEDIATELY")
    // Keep the smoke test cheap/deterministic even if the parent environment has a larger Rayon
    // pool configured.
    .env("RAYON_NUM_THREADS", "1")
    .output()
    .expect("spawn browser");

  let stderr = String::from_utf8_lossy(&output.stderr);
  let stdout = String::from_utf8_lossy(&output.stdout);

  assert!(
    !output.status.success(),
    "expected browser to exit non-zero when the session lock is held; status={:?}\nstderr:\n{}\nstdout:\n{}",
    output.status.code(),
    stderr,
    stdout
  );

  assert!(
    stderr.contains("refusing to start"),
    "expected browser to refuse startup when session is locked; stderr:\n{stderr}\nstdout:\n{stdout}"
  );
  assert!(
    stderr.contains("already in use"),
    "expected browser to mention the session is already in use; stderr:\n{stderr}\nstdout:\n{stdout}"
  );
  let cli_session_display = cli_session_path.to_string_lossy();
  assert!(
    stderr.contains(cli_session_display.as_ref()),
    "expected browser error to mention CLI session path {}; stderr:\n{stderr}\nstdout:\n{stdout}",
    cli_session_path.display()
  );
}
