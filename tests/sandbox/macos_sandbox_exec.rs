//! macOS-only integration test proving the `sandbox-exec` spawn path enforces restrictions.
//!
//! The unit-style macOS sandbox tests exercise `sandbox_init` (in-process). This test instead
//! launches a child test process wrapped in `/usr/bin/sandbox-exec` so the sandbox is applied
//! *before* the Rust test harness starts.

use fastrender::sandbox::macos_spawn::{sandbox_exec_command, SandboxExecError};
use std::ffi::OsString;
use std::io;
use std::net::TcpStream;
use std::path::PathBuf;

const CHILD_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_EXEC_CHILD";
const PORT_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_EXEC_LOCALHOST_PORT";
const WRITE_PATH_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_EXEC_WRITE_PATH";

fn is_permission_denied(err: &io::Error) -> bool {
  matches!(err.kind(), io::ErrorKind::PermissionDenied)
    || matches!(
      err.raw_os_error(),
      Some(libc::EACCES) | Some(libc::EPERM)
    )
}

fn assert_denied<T>(result: io::Result<T>, context: &str) {
  match result {
    Ok(_) => panic!("expected sandbox to deny {context}, but operation succeeded"),
    Err(err) => assert!(
      is_permission_denied(&err),
      "expected sandbox to deny {context} with EACCES/EPERM, got {err:?}"
    ),
  }
}

fn read_passwd() -> io::Result<Vec<u8>> {
  std::fs::read("/private/etc/passwd").or_else(|err| {
    if err.kind() == io::ErrorKind::NotFound {
      std::fs::read("/etc/passwd")
    } else {
      Err(err)
    }
  })
}

fn run_child() {
  let write_path = PathBuf::from(
    std::env::var_os(WRITE_PATH_ENV).expect("missing write probe path env var"),
  );
  let port: u16 = std::env::var(PORT_ENV)
    .expect("missing localhost port env var")
    .parse()
    .expect("parse localhost port");

  assert_denied(read_passwd(), "read /etc/passwd");
  assert_denied(
    std::fs::write(&write_path, b"probe"),
    &format!("write {}", write_path.display()),
  );
  assert_denied(
    TcpStream::connect(("127.0.0.1", port)),
    &format!("connect to 127.0.0.1:{port}"),
  );
}

#[test]
fn sandbox_exec_wrapper_enforces_sandbox() {
  if std::env::var_os(CHILD_ENV).is_some() {
    run_child();
    return;
  }

  // Ensure the sandbox isn't disabled by a developer's env overrides, and restore the previous
  // environment so the consolidated test binary remains deterministic.
  let _env_guard = crate::common::EnvVarsGuard::remove(&[
    "FASTR_DISABLE_RENDERER_SANDBOX",
    "FASTR_RENDERER_SANDBOX",
    "FASTR_MACOS_RENDERER_SANDBOX",
  ]);

  let _net_lock = crate::common::net_test_lock();
  let Some(listener) = crate::common::try_bind_localhost("macos sandbox-exec enforcement test") else {
    return;
  };
  let port = listener
    .local_addr()
    .expect("localhost listener addr")
    .port();

  // Ensure the environment can reach localhost so `PermissionDenied` failures in the sandboxed
  // child are meaningful (not `ECONNREFUSED`).
  if TcpStream::connect(("127.0.0.1", port)).is_err() {
    eprintln!("skipping macos sandbox-exec test: cannot connect to localhost in parent process");
    return;
  }

  if read_passwd().is_err() {
    eprintln!("skipping macos sandbox-exec test: cannot read /etc/passwd in parent process");
    return;
  }

  let tempdir = tempfile::tempdir().expect("create temp dir for sandbox probes");
  let write_probe_path = tempdir.path().join("write_probe.txt");

  let exe = std::env::current_exe().expect("current test exe path");
  let test_name =
    crate::common::libtest::exact_test_name(module_path!(), stringify!(sandbox_exec_wrapper_enforces_sandbox));

  let args = vec![
    OsString::from("--exact"),
    OsString::from(&test_name),
    OsString::from("--test-threads=1"),
    OsString::from("--nocapture"),
  ];
  let mut cmd = match sandbox_exec_command(&exe, &args) {
    Ok(Some(cmd)) => cmd,
    Ok(None) => {
      eprintln!("skipping macos sandbox-exec enforcement test: renderer sandbox disabled");
      return;
    }
    Err(SandboxExecError::MissingSandboxExec { path }) => {
      eprintln!(
        "skipping macos sandbox-exec enforcement test: {} is missing",
        path.display()
      );
      return;
    }
    Err(err) => panic!("failed to construct sandbox-exec command: {err}"),
  };

  let output = cmd
    .env(CHILD_ENV, "1")
    .env(PORT_ENV, port.to_string())
    .env(WRITE_PATH_ENV, &write_probe_path)
    .env("RUST_TEST_THREADS", "1")
    .output()
    .expect("spawn sandbox-exec child test process");

  assert!(
    output.status.success(),
    "sandbox-exec child should exit successfully (status={:?})\nstdout:\n{}\nstderr:\n{}",
    output.status.code(),
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}
