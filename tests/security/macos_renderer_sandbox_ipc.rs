use std::os::unix::net::UnixListener;
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::tempdir;

fn probe_bin() -> &'static str {
  env!("CARGO_BIN_EXE_macos_renderer_sandbox_ipc_probe")
}

#[test]
fn pipes_only_allows_stdio_pipe() {
  let output = Command::new(probe_bin())
    .arg("pipes-only")
    .arg("pipe-stdio")
    .output()
    .expect("spawn probe");

  assert!(
    output.status.success(),
    "probe failed: status={:?} stderr={}",
    output.status.code(),
    String::from_utf8_lossy(&output.stderr)
  );
  assert_eq!(output.stdout, b"ok");
}

#[test]
fn posix_shm_requires_allowance() {
  // POSIX shm names must start with '/' and contain no other slashes.
  let shm_name = format!("/fastrender_sandbox_test_shm_{}_{}", std::process::id(), unique_suffix());

  // Allowed.
  let ok = Command::new(probe_bin())
    .arg("posix-shm")
    .arg("posix-shm-create")
    .arg(&shm_name)
    .output()
    .expect("spawn probe (allowed)");
  assert!(
    ok.status.success(),
    "expected shm_open to succeed, status={:?} stderr={}",
    ok.status.code(),
    String::from_utf8_lossy(&ok.stderr)
  );

  // Denied (no ipc-posix-shm allowance).
  let denied = Command::new(probe_bin())
    .arg("pipes-only")
    .arg("posix-shm-create")
    .arg(&shm_name)
    .output()
    .expect("spawn probe (denied)");
  assert!(
    !denied.status.success(),
    "expected shm_open to fail without PosixShm allowance; stdout={}, stderr={}",
    String::from_utf8_lossy(&denied.stdout),
    String::from_utf8_lossy(&denied.stderr)
  );
}

#[test]
fn unix_socket_requires_allowance() {
  let dir = tempdir().expect("tempdir");
  let socket_path = dir.path().join("renderer-ipc.sock");

  let listener = UnixListener::bind(&socket_path).expect("bind unix socket");
  listener
    .set_nonblocking(true)
    .expect("set_nonblocking");

  // Allowed: should be able to connect.
  let mut child = Command::new(probe_bin())
    .arg("unix-socket")
    .arg("unix-socket-connect")
    .arg(&socket_path)
    .spawn()
    .expect("spawn probe (allowed)");

  // Wait for a connection to land in the accept queue (without hanging forever).
  let start = Instant::now();
  let mut accepted = false;
  while start.elapsed() < Duration::from_secs(2) {
    match listener.accept() {
      Ok((_stream, _addr)) => {
        accepted = true;
        break;
      }
      Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
        std::thread::sleep(Duration::from_millis(10));
      }
      Err(err) => panic!("accept failed: {err}"),
    }
  }

  let status = child.wait().expect("wait probe");
  assert!(status.success(), "expected probe to succeed, got {status:?}");
  assert!(
    accepted,
    "expected unix socket connect to succeed (no connection observed)"
  );

  // Denied: should fail to connect, and no connection should be accepted.
  let denied = Command::new(probe_bin())
    .arg("pipes-only")
    .arg("unix-socket-connect")
    .arg(&socket_path)
    .output()
    .expect("spawn probe (denied)");
  assert!(
    !denied.status.success(),
    "expected connect to fail without UnixSocket allowance; stdout={}, stderr={}",
    String::from_utf8_lossy(&denied.stdout),
    String::from_utf8_lossy(&denied.stderr)
  );

  match listener.accept() {
    Ok(_) => panic!("unexpected unix socket connection accepted in denied mode"),
    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
    Err(err) => panic!("accept failed: {err}"),
  }
}

#[test]
fn mach_lookup_requires_allowance() {
  // This is a well-known launchd Mach service used by many real-world sandbox
  // profiles (e.g. Chromium) as an allowlisted lookup.
  let service = "com.apple.cfprefsd.daemon";

  let ok = Command::new(probe_bin())
    .arg("mach-port")
    .arg("mach-lookup")
    .arg(service)
    .output()
    .expect("spawn probe (allowed)");
  assert!(
    ok.status.success(),
    "expected mach lookup to succeed with MachPort allowance: stdout={}, stderr={}",
    String::from_utf8_lossy(&ok.stdout),
    String::from_utf8_lossy(&ok.stderr)
  );

  let denied = Command::new(probe_bin())
    .arg("pipes-only")
    .arg("mach-lookup")
    .arg(service)
    .output()
    .expect("spawn probe (denied)");
  assert!(
    !denied.status.success(),
    "expected mach lookup to fail without MachPort allowance; stdout={}, stderr={}",
    String::from_utf8_lossy(&denied.stdout),
    String::from_utf8_lossy(&denied.stderr)
  );
}

fn unique_suffix() -> u128 {
  // Avoid `rand` in tests; a monotonic timestamp is sufficient for uniqueness.
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .expect("time")
    .as_nanos()
}
