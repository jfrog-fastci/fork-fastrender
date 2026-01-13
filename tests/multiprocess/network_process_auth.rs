use crate::common::net::{net_test_lock, try_bind_localhost};
use fastrender::network_process::{ipc, spawn_network_process, NetworkProcessConfig};
use std::io::{self, Read};
use std::net::TcpStream;
use std::time::{Duration, Instant};

#[test]
fn network_process_rejects_invalid_auth_token() {
  let _net_guard = net_test_lock();
  if try_bind_localhost("network_process_rejects_invalid_auth_token").is_none() {
    return;
  };

  let handle = spawn_network_process(NetworkProcessConfig {
    inherit_stderr: false,
    ..NetworkProcessConfig::default()
  });
  let addr = handle.addr();

  let mut stream =
    TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect to network process");
  stream.set_nodelay(true).unwrap();

  // `spawn_network_process` provisions a hex token; sending any token that includes non-hex
  // characters is guaranteed to fail authentication deterministically.
  ipc::write_request_frame(
    &mut stream,
    &ipc::NetworkRequest::Hello {
      token: "this-is-not-a-valid-token".to_string(),
      role: ipc::ClientRole::Renderer,
    },
  )
  .expect("write hello frame");

  stream
    .set_read_timeout(Some(Duration::from_millis(100)))
    .unwrap();
  let start = Instant::now();
  let mut buf = [0u8; 1];
  loop {
    match stream.read(&mut buf) {
      Ok(0) => break,
      Ok(n) => panic!("unexpected {n} byte response from unauthenticated connection"),
      Err(err)
        if matches!(
          err.kind(),
          io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted | io::ErrorKind::BrokenPipe
        ) =>
      {
        break
      }
      Err(err) if matches!(err.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) => {
        if start.elapsed() > Duration::from_secs(1) {
          panic!("network process did not close unauthenticated connection in time");
        }
        std::thread::sleep(Duration::from_millis(10));
      }
      Err(err) => panic!("unexpected read error from unauthenticated connection: {err}"),
    }
  }
}
