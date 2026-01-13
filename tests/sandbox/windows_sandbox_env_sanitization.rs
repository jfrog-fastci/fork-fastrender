#![cfg(target_os = "windows")]

use std::path::PathBuf;
use std::process::Command;

use fastrender::sandbox::windows::spawn_sandboxed;

#[test]
fn sandboxed_child_does_not_inherit_parent_environment_by_default() {
  const CHILD_ENV: &str = "FASTR_TEST_WINDOWS_SANDBOX_ENV_SANITIZATION_CHILD";
  const MODE_ENV: &str = "FASTR_TEST_WINDOWS_SANDBOX_ENV_SANITIZATION_MODE";
  const SECRET_ENV: &str = "FASTR_SECRET_SHOULD_NOT_LEAK";
  const INHERIT_ENV: &str = "FASTR_WINDOWS_SANDBOX_INHERIT_ENV";
  const TEST_NAME: &str = concat!(
    module_path!(),
    "::sandboxed_child_does_not_inherit_parent_environment_by_default"
  );

  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    let mode = std::env::var(MODE_ENV).expect("child missing mode env");
    let probe_path = PathBuf::from(env!("CARGO_BIN_EXE_sandbox_env_probe"));
    let exit_code = spawn_sandboxed(&probe_path, &[], &[])
      .expect("spawn sandboxed child")
      .wait()
      .expect("wait for sandboxed child");

    match mode.as_str() {
      "no_inherit" => assert_eq!(
        exit_code, 0,
        "expected sandboxed child to not see {SECRET_ENV} by default"
      ),
      "inherit" => assert_eq!(
        exit_code, 1,
        "expected sandboxed child to see {SECRET_ENV} when {INHERIT_ENV}=1"
      ),
      other => panic!("unknown env sanitization mode {other:?}"),
    }
    return;
  }

  let exe = std::env::current_exe().expect("current test exe path");
  for mode in ["no_inherit", "inherit"] {
    let mut cmd = Command::new(&exe);
    cmd
      .env(CHILD_ENV, "1")
      .env(MODE_ENV, mode)
      .env(SECRET_ENV, "1")
      // Keep this test runnable even on Windows hosts where AppContainer is unavailable: we only
      // care about environment inheritance behavior, which is enforced for all spawn modes.
      .env("FASTR_DISABLE_RENDERER_SANDBOX", "1")
      .env_remove("FASTR_WINDOWS_RENDERER_SANDBOX")
      .env_remove("FASTR_ALLOW_UNSANDBOXED_RENDERER")
      // Keep the subprocess deterministic.
      .env("RUST_TEST_THREADS", "1");
    match mode {
      "no_inherit" => {
        cmd.env_remove(INHERIT_ENV);
      }
      "inherit" => {
        cmd.env(INHERIT_ENV, "1");
      }
      _ => unreachable!(),
    }
    let output = cmd
      .arg("--exact")
      .arg(TEST_NAME)
      .arg("--nocapture")
      .output()
      .expect("spawn env-sanitization helper process");

    assert!(
      output.status.success(),
      "helper subprocess ({mode}) should exit successfully (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }
}
