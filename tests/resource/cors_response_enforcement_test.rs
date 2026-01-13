use crate::common::net::{net_test_lock, try_bind_localhost};
use fastrender::error::Error;
use fastrender::resource::{origin_from_url, FetchDestination, FetchRequest, HttpFetcher};
use fastrender::ResourceFetcher;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{Duration, Instant};

const MAX_WAIT: Duration = Duration::from_secs(3);

fn spawn_server(listener: TcpListener, allow_origin: Option<&'static str>) -> thread::JoinHandle<()> {
  thread::spawn(move || {
    let _ = listener.set_nonblocking(true);
    let start = Instant::now();
    while start.elapsed() < MAX_WAIT {
      match listener.accept() {
        Ok((mut stream, _)) => {
          let mut buf = Vec::new();
          let mut tmp = [0u8; 1024];
          loop {
            match stream.read(&mut tmp) {
              Ok(0) => break,
              Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                  break;
                }
              }
              Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
              }
              Err(_) => break,
            }
          }

          let request = String::from_utf8_lossy(&buf).to_ascii_lowercase();
          assert!(
            request.contains("sec-fetch-mode: cors"),
            "expected CORS-mode request, got: {request}"
          );
          assert!(
            request.contains("origin: https://client.example"),
            "expected Origin header on CORS-mode request, got: {request}"
          );

          let body = b"secret";
          let mut response =
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n".to_string();
          if let Some(allow_origin) = allow_origin {
            response.push_str(&format!("Access-Control-Allow-Origin: {allow_origin}\r\n"));
          }
          response.push_str(&format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
          ));
          let _ = stream.write_all(response.as_bytes());
          let _ = stream.write_all(body);
          return;
        }
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
          thread::sleep(Duration::from_millis(5));
        }
        Err(_) => break,
      }
    }
  })
}

#[test]
fn network_cors_blocks_cross_origin_response_without_allow_origin() {
  let _net_guard = net_test_lock();
  let Some(listener) =
    try_bind_localhost("network_cors_blocks_cross_origin_response_without_allow_origin")
  else {
    return;
  };
  let addr = listener.local_addr().unwrap();
  let handle = spawn_server(listener, None);

  let client_origin = origin_from_url("https://client.example/").expect("origin");
  let url = format!("http://{addr}/resource");
  let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
  let req = FetchRequest::new(&url, FetchDestination::Fetch).with_client_origin(&client_origin);

  let err = fetcher
    .fetch_with_request(req)
    .expect_err("expected network-side CORS enforcement to block response");
  match err {
    Error::Resource(err) => {
      assert!(
        err.message.contains("Access-Control-Allow-Origin"),
        "unexpected error message: {}",
        err.message
      );
    }
    other => panic!("expected Resource error, got {other:?}"),
  }

  handle.join().unwrap();
}

#[test]
fn network_cors_allows_cross_origin_response_with_matching_allow_origin() {
  let _net_guard = net_test_lock();
  let Some(listener) =
    try_bind_localhost("network_cors_allows_cross_origin_response_with_matching_allow_origin")
  else {
    return;
  };
  let addr = listener.local_addr().unwrap();
  let handle = spawn_server(listener, Some("https://client.example"));

  let client_origin = origin_from_url("https://client.example/").expect("origin");
  let url = format!("http://{addr}/resource");
  let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
  let req = FetchRequest::new(&url, FetchDestination::Fetch).with_client_origin(&client_origin);

  let res = fetcher
    .fetch_with_request(req)
    .expect("expected matching ACAO to allow response bytes");
  assert_eq!(res.bytes, b"secret");

  handle.join().unwrap();
}

