use crate::common::net::{net_test_lock, try_bind_localhost};
use fastrender::ipc::IpcFetchServer;
use fastrender::resource::{FetchDestination, FetchRequest, HttpFetcher};
use fastrender::{IpcResourceFetcher, ResourceFetcher};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

const MAX_WAIT: Duration = Duration::from_secs(3);
const TEST_AUTH_TOKEN: &str = "fastrender-ipc-test-token";

fn read_http_request_headers(stream: &mut TcpStream) -> String {
  let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
  let mut buf = Vec::new();
  let mut tmp = [0u8; 1024];
  let start = Instant::now();
  while start.elapsed() < MAX_WAIT {
    match stream.read(&mut tmp) {
      Ok(0) => break,
      Ok(n) => {
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
          break;
        }
      }
      Err(err) if matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
        thread::sleep(Duration::from_millis(5));
      }
      Err(_) => break,
    }
  }
  String::from_utf8_lossy(&buf).to_string()
}

fn respond_ok(stream: &mut TcpStream, body: &[u8]) {
  let response = format!(
    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
    body.len()
  );
  stream.write_all(response.as_bytes()).unwrap();
  stream.write_all(body).unwrap();
}

fn spawn_ipc_fetch_server(listener: TcpListener) -> thread::JoinHandle<()> {
  thread::spawn(move || {
    let (mut stream, _) = listener.accept().expect("accept ipc client");
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let mut server = IpcFetchServer::new(fetcher, TEST_AUTH_TOKEN).expect("create IpcFetchServer");
    let reader = stream.try_clone().expect("clone ipc stream");
    server.run(reader, stream).expect("run IpcFetchServer");
  })
}

#[test]
#[cfg(feature = "direct_network")]
fn ipc_fetch_server_supports_simple_get() {
  let _net_guard = net_test_lock();
  let Some(http_listener) = try_bind_localhost("ipc_fetch_server_supports_simple_get (http)") else {
    return;
  };
  let http_addr = http_listener.local_addr().unwrap();

  let Some(ipc_listener) = try_bind_localhost("ipc_fetch_server_supports_simple_get (ipc)") else {
    return;
  };
  let ipc_addr = ipc_listener.local_addr().unwrap();

  let http_handle = thread::spawn(move || {
    let (mut stream, _) = http_listener.accept().expect("accept http client");
    let _ = read_http_request_headers(&mut stream);
    respond_ok(&mut stream, b"hello");
  });

  let ipc_handle = spawn_ipc_fetch_server(ipc_listener);

  let fetcher =
    IpcResourceFetcher::new_with_auth_token(ipc_addr.to_string(), TEST_AUTH_TOKEN).expect("connect ipc fetcher");
  let url = format!("http://{http_addr}/");
  let res = fetcher.fetch(&url).expect("fetch via ipc");
  assert_eq!(res.bytes, b"hello");

  drop(fetcher);
  http_handle.join().unwrap();
  ipc_handle.join().unwrap();
}

#[test]
#[cfg(feature = "direct_network")]
fn ipc_fetch_server_supports_fetch_partial_with_request() {
  let _net_guard = net_test_lock();
  let Some(http_listener) =
    try_bind_localhost("ipc_fetch_server_supports_fetch_partial_with_request (http)")
  else {
    return;
  };
  let http_addr = http_listener.local_addr().unwrap();

  let Some(ipc_listener) =
    try_bind_localhost("ipc_fetch_server_supports_fetch_partial_with_request (ipc)")
  else {
    return;
  };
  let ipc_addr = ipc_listener.local_addr().unwrap();

  let body = b"abcdefghijklmnopqrstuvwxyz".to_vec();
  let http_handle = thread::spawn(move || {
    let (mut stream, _) = http_listener.accept().expect("accept http client");
    let _ = read_http_request_headers(&mut stream);
    respond_ok(&mut stream, &body);
  });

  let ipc_handle = spawn_ipc_fetch_server(ipc_listener);

  let fetcher =
    IpcResourceFetcher::new_with_auth_token(ipc_addr.to_string(), TEST_AUTH_TOKEN).expect("connect ipc fetcher");
  let url = format!("http://{http_addr}/partial");
  let req = FetchRequest::new(&url, FetchDestination::Fetch);
  let res = fetcher
    .fetch_partial_with_request(req, 3)
    .expect("fetch_partial_with_request via ipc");
  assert_eq!(res.bytes, b"abc");

  drop(fetcher);
  http_handle.join().unwrap();
  ipc_handle.join().unwrap();
}

#[test]
#[cfg(feature = "direct_network")]
fn ipc_fetch_server_cookie_store_round_trips_into_cookie_header_value() {
  let _net_guard = net_test_lock();
  let Some(ipc_listener) =
    try_bind_localhost("ipc_fetch_server_cookie_store_round_trips_into_cookie_header_value")
  else {
    return;
  };
  let ipc_addr = ipc_listener.local_addr().unwrap();

  let ipc_handle = spawn_ipc_fetch_server(ipc_listener);

  let fetcher =
    IpcResourceFetcher::new_with_auth_token(ipc_addr.to_string(), TEST_AUTH_TOKEN).expect("connect ipc fetcher");

  let url = "http://example.com/".to_string();
  let before = fetcher
    .cookie_header_value(&url)
    .expect("cookie_header_value should be Some for valid URL");
  assert!(before.is_empty(), "expected no cookies, got {before:?}");

  fetcher.store_cookie_from_document(&url, "a=b; Path=/");

  let cookies = fetcher
    .cookie_header_value(&url)
    .expect("cookie_header_value should be Some for valid URL");
  assert!(cookies.contains("a=b"), "expected a=b, got {cookies:?}");

  drop(fetcher);
  ipc_handle.join().unwrap();
}

