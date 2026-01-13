#![cfg(target_os = "windows")]

use std::path::PathBuf;

use fastrender::sandbox::windows::spawn_sandboxed;

#[test]
fn sandboxed_child_does_not_inherit_parent_environment_by_default() {
  const SECRET_ENV: &str = "FASTR_SECRET_SHOULD_NOT_LEAK";
  const INHERIT_ENV: &str = "FASTR_WINDOWS_SANDBOX_INHERIT_ENV";

  let probe_path = PathBuf::from(env!("CARGO_BIN_EXE_sandbox_env_probe"));

  let prev_secret = std::env::var_os(SECRET_ENV);
  let prev_inherit = std::env::var_os(INHERIT_ENV);

  std::env::set_var(SECRET_ENV, "1");
  std::env::remove_var(INHERIT_ENV);

  let exit_code = spawn_sandboxed(&probe_path, &[], &[])
    .expect("spawn sandboxed child")
    .wait()
    .expect("wait for sandboxed child");

  assert_eq!(
    exit_code, 0,
    "expected sandboxed child to not see {SECRET_ENV} by default"
  );

  std::env::set_var(INHERIT_ENV, "1");

  let exit_code = spawn_sandboxed(&probe_path, &[], &[])
    .expect("spawn sandboxed child with inherited env")
    .wait()
    .expect("wait for sandboxed child");

  assert_eq!(
    exit_code, 1,
    "expected sandboxed child to see {SECRET_ENV} when {INHERIT_ENV}=1"
  );

  match prev_secret {
    Some(val) => std::env::set_var(SECRET_ENV, val),
    None => std::env::remove_var(SECRET_ENV),
  }
  match prev_inherit {
    Some(val) => std::env::set_var(INHERIT_ENV, val),
    None => std::env::remove_var(INHERIT_ENV),
  }
}
