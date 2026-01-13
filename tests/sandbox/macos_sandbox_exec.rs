//! macOS-only integration test proving the `sandbox-exec` spawn path enforces restrictions.
//!
//! The unit-style macOS sandbox tests exercise `sandbox_init` (in-process). This test instead
//! launches a child test process wrapped in `/usr/bin/sandbox-exec` so the sandbox is applied
//! *before* the Rust test harness starts.

use std::io;
use std::net::TcpListener;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;

const CHILD_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_EXEC_CHILD";
const PORT_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_EXEC_LOCALHOST_PORT";
const READ_PATH_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_EXEC_READ_PATH";
const WRITE_PATH_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_EXEC_WRITE_PATH";

const TEST_NAME: &str = stringify!(sandbox_exec_wrapper_enforces_sandbox);

fn sandbox_exec_path() -> &'static Path {
  Path::new("/usr/bin/sandbox-exec")
}

fn sandbox_profile(read_path: &Path, write_path: &Path) -> String {
  fn escape_sbpl_string(value: &str) -> String {
    // SBPL uses C-like string literals. The generated temp paths should be "boring", but escape
    // `"` defensively so the profile remains parseable.
    value.replace('"', "\\\"")
  }

  let read_path = escape_sbpl_string(&read_path.to_string_lossy());
  let write_path = escape_sbpl_string(&write_path.to_string_lossy());

  // Default policy for sandbox-exec profiles is allow; we install a few targeted denies so the
  // child process remains runnable while still proving sandbox enforcement.
  format!(
    "(version 1)\n\
     (deny file-read* (literal \"{read_path}\"))\n\
     (deny file-write* (literal \"{write_path}\"))\n\
     (deny network-outbound)\n\
     (deny network-inbound)\n"
  )
}

fn sandbox_exec_command(program: &Path, profile: &str) -> Command {
  let mut cmd = Command::new(sandbox_exec_path());
  cmd.arg("-p").arg(profile).arg(program);
  cmd
}

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

fn run_child() {
  let read_path = PathBuf::from(
    std::env::var_os(READ_PATH_ENV).expect("missing read probe path env var"),
  );
  let write_path = PathBuf::from(
    std::env::var_os(WRITE_PATH_ENV).expect("missing write probe path env var"),
  );
  let port: u16 = std::env::var(PORT_ENV)
    .expect("missing localhost port env var")
    .parse()
    .expect("parse localhost port");

  assert_denied(std::fs::read(&read_path), &format!("read {}", read_path.display()));
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

  let sandbox_exec = sandbox_exec_path();
  if !sandbox_exec.is_file() {
    eprintln!(
      "skipping macos sandbox-exec enforcement test: {} is missing",
      sandbox_exec.display()
    );
    return;
  }

  let Ok(listener) = TcpListener::bind(("127.0.0.1", 0)) else {
    eprintln!("skipping macos sandbox-exec enforcement test: unable to bind localhost");
    return;
  };
  let port = listener
    .local_addr()
    .expect("localhost listener addr")
    .port();

  let tempdir = tempfile::tempdir().expect("create temp dir for sandbox probes");
  let read_probe_path = tempdir.path().join("read_probe.txt");
  std::fs::write(&read_probe_path, b"read-probe").expect("create read probe file in parent");
  let write_probe_path = tempdir.path().join("write_probe.txt");

  let profile = sandbox_profile(&read_probe_path, &write_probe_path);
  let exe = std::env::current_exe().expect("current test exe path");

  let output = sandbox_exec_command(&exe, &profile)
    .env(CHILD_ENV, "1")
    .env(PORT_ENV, port.to_string())
    .env(READ_PATH_ENV, &read_probe_path)
    .env(WRITE_PATH_ENV, &write_probe_path)
    .arg("--exact")
    .arg(TEST_NAME)
    .arg("--nocapture")
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
