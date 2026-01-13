use crate::common::net::{net_test_lock, try_bind_localhost};
use fastrender::network_process::{ipc, spawn_network_process, NetworkProcessConfig};
use std::net::TcpStream;
use std::time::Duration;

#[test]
fn network_process_denies_download_start_for_renderer_role() {
  let _net_guard = net_test_lock();
  if try_bind_localhost("network_process_denies_download_start_for_renderer_role").is_none() {
    return;
  };

  let handle = spawn_network_process(NetworkProcessConfig {
    inherit_stderr: false,
    ..NetworkProcessConfig::default()
  });

  let mut stream =
    TcpStream::connect_timeout(&handle.addr(), Duration::from_secs(2)).expect("connect to network");
  stream.set_nodelay(true).unwrap();
  stream
    .set_read_timeout(Some(Duration::from_secs(2)))
    .unwrap();
  stream
    .set_write_timeout(Some(Duration::from_secs(2)))
    .unwrap();

  let mut conn = ipc::NetworkClient::new(stream);
  conn
    .send_request(&ipc::NetworkRequest::Hello {
      token: handle.auth_token().to_string(),
      role: ipc::ClientRole::Renderer,
    })
    .expect("send hello");

  let ack: ipc::NetworkResponse = conn.recv_response().expect("recv hello ack");
  assert!(matches!(ack, ipc::NetworkResponse::HelloAck));

  conn
    .send_request(&ipc::NetworkRequest::DownloadStart {
      // Use an invalid URL so that a missing permission check cannot accidentally hit the network.
      url: "not a url".to_string(),
    })
    .expect("send download_start");

  let resp: ipc::NetworkResponse = conn.recv_response().expect("recv download_start response");
  assert!(matches!(
    resp,
    ipc::NetworkResponse::Error {
      error: ipc::NetworkError::PermissionDenied
    }
  ));
}

