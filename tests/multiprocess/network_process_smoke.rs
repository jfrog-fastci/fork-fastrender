use crate::common::net::{net_test_lock, try_bind_localhost};
use fastrender::network_process::{spawn_network_process, NetworkProcessConfig};
use fastrender::ResourceFetcher;
use std::net::TcpStream;
use std::time::{Duration, Instant};

#[cfg(unix)]
fn pid_exists(pid: u32) -> bool {
  use std::io;
  if pid == 0 || pid > i32::MAX as u32 {
    return false;
  }
  // SAFETY: libc call with correct types; signal 0 checks for existence without sending a signal.
  let res = unsafe { libc::kill(pid as i32, 0) };
  if res == 0 {
    return true;
  }
  let err = io::Error::last_os_error();
  match err.raw_os_error() {
    Some(code) if code == libc::ESRCH => false,
    Some(code) if code == libc::EPERM => true,
    _ => true,
  }
}

#[cfg(not(unix))]
fn pid_exists(_pid: u32) -> bool {
  false
}

#[test]
fn network_process_smoke_fetch_and_drop_exits() {
  let _net_guard = net_test_lock();
  if try_bind_localhost("network_process_smoke_fetch_and_drop_exits").is_none() {
    return;
  };

  let handle = spawn_network_process(NetworkProcessConfig {
    inherit_stderr: false,
    ..NetworkProcessConfig::default()
  });
  let pid = handle.pid();
  let addr = handle.addr();

  let client = handle.connect_client();
  let fetcher = client.resource_fetcher();

  let res = fetcher
    .fetch("data:text/plain;base64,aGVsbG8=")
    .expect("fetch data URL via network process");
  assert_eq!(res.bytes, b"hello");

  drop(handle);

  let start = Instant::now();
  while pid_exists(pid) && start.elapsed() < Duration::from_secs(2) {
    std::thread::sleep(Duration::from_millis(10));
  }
  assert!(
    !pid_exists(pid),
    "network process (pid {pid}) should exit when handle is dropped"
  );

  // The listening socket should be gone as well.
  assert!(
    TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err(),
    "expected network process listener {addr} to be closed after drop"
  );
}
