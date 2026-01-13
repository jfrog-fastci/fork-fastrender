#![cfg(target_os = "macos")]

use fastrender::sandbox_exec::{SandboxExecCommand, SandboxExecProfile};
use std::io;
use std::net::{TcpListener, TcpStream};
use std::process::Stdio;

const ENV_CHILD: &str = "FASTR_SANDBOX_EXEC_CUSTOM_PROFILE_CHILD";
const ENV_SECRET_PATH: &str = "FASTR_SANDBOX_EXEC_SECRET_PATH";
const ENV_PORT: &str = "FASTR_SANDBOX_EXEC_PORT";
const TEST_NAME: &str = concat!(
  module_path!(),
  "::sandbox_exec_custom_profile_denies_file_and_network"
);

fn seatbelt_escape_string(value: &str) -> String {
  value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn is_permission_denied(err: &io::Error) -> bool {
  if err.kind() == io::ErrorKind::PermissionDenied {
    return true;
  }
  match err.raw_os_error() {
    Some(code) => code == libc::EPERM || code == libc::EACCES,
    None => false,
  }
}

#[test]
fn sandbox_exec_custom_profile_denies_file_and_network() {
  let is_child = std::env::var_os(ENV_CHILD).is_some();
  if is_child {
    let secret_path =
      std::env::var(ENV_SECRET_PATH).expect("missing env: FASTR_SANDBOX_EXEC_SECRET_PATH");
    let port: u16 = std::env::var(ENV_PORT)
      .expect("missing env: FASTR_SANDBOX_EXEC_PORT")
      .parse()
      .expect("parse port");

    let err = std::fs::read_to_string(&secret_path)
      .expect_err("sandbox should deny reading secret file");
    assert!(
      is_permission_denied(&err),
      "expected PermissionDenied from reading {secret_path}, got: {err:?}"
    );

    let err = TcpStream::connect(("127.0.0.1", port))
      .expect_err("sandbox should deny connecting to localhost");
    assert!(
      is_permission_denied(&err),
      "expected PermissionDenied from TCP connect, got: {err:?}"
    );
    return;
  }

  // Set up a secret file that the sandboxed child should *not* be able to read.
  let temp = tempfile::tempdir().expect("tempdir");
  let secret_path = temp.path().join("secret.txt");
  std::fs::write(&secret_path, "super secret").expect("write secret file");

  // Start a localhost listener so the TCP connect succeeds when not sandboxed.
  let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind listener");
  let port = listener.local_addr().expect("local addr").port();

  // Deny reading the secret file and deny network connections. Start from allow-default so the
  // Rust test binary itself can still start up.
  let profile = format!(
    "(version 1)\n\
     (allow default)\n\
     (deny file-read* (literal \"{}\"))\n\
     (deny network-outbound)\n\
     (deny network-inbound)\n",
    seatbelt_escape_string(&secret_path.to_string_lossy())
  );

  let exe = std::env::current_exe().expect("current test exe");
  let mut cmd = SandboxExecCommand::new(
    SandboxExecProfile::Custom(profile),
    &exe,
    ["--exact", TEST_NAME, "--nocapture"],
  )
  .expect("build sandbox-exec command");
  cmd
    .command_mut()
    .env(ENV_CHILD, "1")
    .env(ENV_SECRET_PATH, secret_path)
    .env(ENV_PORT, port.to_string())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

  // Prove custom SBPL profiles written to temp files stay alive until spawn.
  std::thread::sleep(std::time::Duration::from_millis(200));

  let output = cmd.output().expect("run sandboxed child");
  assert!(
    output.status.success(),
    "sandboxed child should succeed (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  drop(listener);
}
