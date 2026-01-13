use fastrender::network_process::{spawn_network_process, NetworkProcessConfig};
use fastrender::ResourceFetcher;
use std::io;
use std::net::TcpListener;
use std::net::TcpStream;
use std::time::{Duration, Instant};

fn try_bind_localhost(context: &str) -> Option<TcpListener> {
  match TcpListener::bind("127.0.0.1:0") {
    Ok(listener) => Some(listener),
    Err(err)
      if matches!(
        err.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::AddrNotAvailable
      ) =>
    {
      eprintln!("skipping {context}: cannot bind localhost in this environment: {err}");
      None
    }
    Err(err) => panic!("bind {context}: {err}"),
  }
}

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
fn network_process_smoke() {
  if try_bind_localhost("network_process_smoke").is_none() {
    return;
  };

  let handle = spawn_network_process(NetworkProcessConfig {
    inherit_stderr: false,
    ..NetworkProcessConfig::default()
  });
  let addr = handle.addr();
  let pid = handle.pid();

  let client = handle.connect_client();
  let fetcher = client.resource_fetcher();

  // Use a data: URL so the test doesn't require outbound networking and works under
  // `--no-default-features`.
  let res = fetcher
    .fetch("data:text/plain;base64,aGVsbG8=")
    .expect("fetch data URL via network process");
  assert_eq!(res.bytes, b"hello");

  drop(handle);

  // Confirm the child process exits shortly after dropping the handle.
  let start = Instant::now();
  while pid_exists(pid) && start.elapsed() < Duration::from_secs(2) {
    std::thread::sleep(Duration::from_millis(10));
  }
  assert!(
    !pid_exists(pid),
    "network process (pid {pid}) should exit when handle is dropped"
  );

  // And the listening socket should be gone too (give it a moment; some platforms may still accept
  // connections briefly after we drop/kill the child).
  let deadline = Instant::now() + Duration::from_secs(2);
  loop {
    if TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_err() {
      break;
    }
    if Instant::now() >= deadline {
      panic!("expected network process listener {addr} to be closed after drop");
    }
    std::thread::sleep(Duration::from_millis(10));
  }
}
