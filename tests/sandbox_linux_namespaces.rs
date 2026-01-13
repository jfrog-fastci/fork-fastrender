#![cfg(target_os = "linux")]

use std::process::Command;

#[test]
fn linux_namespaces_best_effort() {
  const CHILD_ENV: &str = "FASTR_TEST_LINUX_NAMESPACES_CHILD";
  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    let status = fastrender::sandbox::linux_namespaces::apply_namespaces(
      fastrender::sandbox::linux_namespaces::LinuxNamespacesConfig {
        enabled: true,
        isolate_mount_namespace: true,
      },
    );

    assert!(
      matches!(
        status,
        fastrender::sandbox::SandboxStatus::Applied | fastrender::sandbox::SandboxStatus::Unsupported
      ),
      "unexpected namespace sandbox status: {status:?}"
    );

    if status == fastrender::sandbox::SandboxStatus::Applied {
      // A fresh network namespace should have no routes/interfaces configured, so outbound
      // connections should fail even without seccomp filters. Keep the assertion tolerant: any
      // error is acceptable, but success would indicate the sandbox didn't isolate networking.
      use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
      use std::time::Duration;

      let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 80);
      let connect = TcpStream::connect_timeout(&addr, Duration::from_millis(200));
      assert!(
        connect.is_err(),
        "expected TCP connect to fail inside isolated net namespace"
      );
    }
    return;
  }

  let exe = std::env::current_exe().expect("current test executable path");
  let output = Command::new(exe)
    .env(CHILD_ENV, "1")
    .env("RUST_TEST_THREADS", "1")
    .arg("--exact")
    .arg("linux_namespaces_best_effort")
    .arg("--nocapture")
    .output()
    .expect("spawn linux namespace sandbox child process");
  assert!(
    output.status.success(),
    "child process should exit successfully (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}

