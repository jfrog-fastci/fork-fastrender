use crate::common::net::{net_test_lock, try_bind_localhost};
use fastrender::resource::ipc_fetcher::{
  validate_ipc_request, BrowserToNetwork, IpcRequest, IpcResponse, IpcResult, NetworkService,
};
use fastrender::resource::HttpFetcher;
use fastrender::{IpcResourceFetcher, ResourceFetcher};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_WAIT: Duration = Duration::from_secs(3);
const TEST_AUTH_TOKEN: &str = "fastrender-ipc-test-token";

fn write_frame(stream: &mut TcpStream, payload: &[u8]) -> io::Result<()> {
  let len = (payload.len() as u32).to_le_bytes();
  stream.write_all(&len)?;
  stream.write_all(payload)?;
  stream.flush()?;
  Ok(())
}

fn read_frame(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
  let mut len_buf = [0u8; 4];
  stream.read_exact(&mut len_buf)?;
  let len = u32::from_le_bytes(len_buf) as usize;
  let mut buf = vec![0u8; len];
  stream.read_exact(&mut buf)?;
  Ok(buf)
}

fn read_http_request_headers(stream: &mut TcpStream) -> io::Result<String> {
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
      Err(ref err)
        if matches!(
          err.kind(),
          io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ) =>
      {
        thread::sleep(Duration::from_millis(5));
      }
      Err(err) => return Err(err),
    }
  }
  let end = buf
    .windows(4)
    .position(|w| w == b"\r\n\r\n")
    .map(|idx| idx + 4)
    .unwrap_or(buf.len());
  Ok(String::from_utf8_lossy(&buf[..end]).to_string())
}

fn parse_request_headers(raw: &str) -> HashMap<String, String> {
  let mut out = HashMap::new();
  for line in raw.split("\r\n").skip(1) {
    if line.is_empty() {
      break;
    }
    let Some((name, value)) = line.split_once(':') else {
      continue;
    };
    out.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
  }
  out
}

fn respond_ok(stream: &mut TcpStream, body: &[u8], extra_headers: &[(&str, &str)]) {
  let mut response = String::from("HTTP/1.1 200 OK\r\n");
  response.push_str("Content-Type: text/plain\r\n");
  for (name, value) in extra_headers {
    response.push_str(name);
    response.push_str(": ");
    response.push_str(value);
    response.push_str("\r\n");
  }
  response.push_str(&format!(
    "Content-Length: {}\r\nConnection: close\r\n\r\n",
    body.len()
  ));
  stream.write_all(response.as_bytes()).unwrap();
  stream.write_all(body).unwrap();
}

fn spawn_network_process(listener: TcpListener, expected_requests: usize) -> thread::JoinHandle<()> {
  thread::spawn(move || {
    let _ = listener.set_nonblocking(true);
    let start = Instant::now();
    let mut accepted: Option<TcpStream> = None;
    while start.elapsed() < MAX_WAIT {
      match listener.accept() {
        Ok((stream, _)) => {
          accepted = Some(stream);
          break;
        }
        Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
          thread::sleep(Duration::from_millis(5));
        }
        Err(err) => panic!("ipc accept failed: {err}"),
      }
    }
    let mut stream = accepted.expect("ipc accept timed out");
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

    // Auth handshake must precede any other IPC request.
    let hello_bytes = read_frame(&mut stream).expect("read ipc hello frame");
    let hello: IpcRequest = serde_json::from_slice(&hello_bytes).expect("decode ipc hello request");
    match hello {
      IpcRequest::Hello { token } => {
        assert_eq!(token, TEST_AUTH_TOKEN, "unexpected IPC auth token");
      }
      other => panic!("expected IPC hello request, got {other:?}"),
    }
    let hello_ack = serde_json::to_vec(&IpcResponse::HelloAck).expect("encode ipc hello ack");
    write_frame(&mut stream, &hello_ack).expect("write ipc hello ack");

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    for _ in 0..expected_requests {
      let req_bytes = read_frame(&mut stream).expect("read ipc frame");
      let env: BrowserToNetwork = serde_json::from_slice(&req_bytes).expect("decode ipc request");
      validate_ipc_request(&env.request).expect("validate ipc request");

      let mut service = NetworkService::new(&mut stream);
      match env.request {
        IpcRequest::Fetch { url } => {
          service
            .send_fetch_result(env.id, fetcher.fetch(&url))
            .expect("write ipc response");
        }
        IpcRequest::CookieHeaderValue { url } => {
          let value = fetcher.cookie_header_value(&url);
          let response = IpcResponse::MaybeString(IpcResult::Ok(value));
          service
            .send_response(env.id, response)
            .expect("write ipc response");
        }
        IpcRequest::StoreCookieFromDocument { url, cookie_string } => {
          fetcher.store_cookie_from_document(&url, &cookie_string);
          let response = IpcResponse::Unit(IpcResult::Ok(()));
          service
            .send_response(env.id, response)
            .expect("write ipc response");
        }
        other => panic!("unexpected IPC request: {other:?}"),
      }
    }
  })
}

#[test]
#[cfg(feature = "direct_network")]
fn ipc_fetcher_basic_get_matches_http_fetcher_bytes_and_content_type() {
  let _net_guard = net_test_lock();
  let Some(http_listener) =
    try_bind_localhost("ipc_fetcher_basic_get_matches_http_fetcher_bytes_and_content_type")
  else {
    return;
  };
  let addr = http_listener.local_addr().unwrap();

  let Some(ipc_listener) =
    try_bind_localhost("ipc_fetcher_basic_get_matches_http_fetcher_bytes_and_content_type (ipc)")
  else {
    return;
  };
  let ipc_addr = ipc_listener.local_addr().unwrap();

  let http_handle = thread::spawn(move || {
    let _ = http_listener.set_nonblocking(true);
    let start = Instant::now();
    while start.elapsed() < MAX_WAIT {
      match http_listener.accept() {
        Ok((mut stream, _)) => {
          let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
          let _ = read_http_request_headers(&mut stream);
          respond_ok(&mut stream, b"hello", &[]);
          return;
        }
        Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
          thread::sleep(Duration::from_millis(5));
        }
        Err(err) => panic!("accept failed: {err}"),
      }
    }
    panic!("http server accept timed out");
  });

  let ipc_handle = spawn_network_process(ipc_listener, 1);

  let fetcher =
    IpcResourceFetcher::new_with_auth_token(ipc_addr.to_string(), TEST_AUTH_TOKEN).expect("connect ipc fetcher");
  let url = format!("http://{addr}/");
  let res = fetcher.fetch(&url).expect("ipc fetch");
  assert_eq!(res.bytes, b"hello");
  assert_eq!(res.content_type.as_deref(), Some("text/plain"));

  drop(fetcher);
  http_handle.join().unwrap();
  ipc_handle.join().unwrap();
}

#[test]
#[cfg(feature = "direct_network")]
fn ipc_fetcher_cookies_round_trip_between_fetch_and_cookie_header_value() {
  let _net_guard = net_test_lock();
  let Some(http_listener) =
    try_bind_localhost("ipc_fetcher_cookies_round_trip_between_fetch_and_cookie_header_value")
  else {
    return;
  };
  let addr = http_listener.local_addr().unwrap();

  let Some(ipc_listener) = try_bind_localhost(
    "ipc_fetcher_cookies_round_trip_between_fetch_and_cookie_header_value (ipc)",
  ) else {
    return;
  };
  let ipc_addr = ipc_listener.local_addr().unwrap();

  let (cookie_tx, cookie_rx) = mpsc::channel::<Option<String>>();
  let http_handle = thread::spawn(move || {
    let _ = http_listener.set_nonblocking(true);
    let start = Instant::now();
    let mut handled = 0usize;
    while handled < 2 && start.elapsed() < MAX_WAIT {
      match http_listener.accept() {
        Ok((mut stream, _)) => {
          let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
          let headers = read_http_request_headers(&mut stream).expect("read request headers");
          let parsed = parse_request_headers(&headers);
          let cookie = parsed.get("cookie").cloned();
          cookie_tx.send(cookie).unwrap();

          if handled == 0 {
            respond_ok(&mut stream, b"first", &[("Set-Cookie", "a=b; Path=/")]);
          } else {
            respond_ok(&mut stream, b"second", &[]);
          }
          handled += 1;
        }
        Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
          thread::sleep(Duration::from_millis(5));
        }
        Err(err) => panic!("accept failed: {err}"),
      }
    }
    if handled < 2 {
      panic!("http server did not receive expected requests (handled {handled})");
    }
  });

  // cookie_header_value (empty), fetch, cookie_header_value, store_cookie_from_document,
  // cookie_header_value, fetch.
  let ipc_handle = spawn_network_process(ipc_listener, 6);

  let fetcher =
    IpcResourceFetcher::new_with_auth_token(ipc_addr.to_string(), TEST_AUTH_TOKEN).expect("connect ipc fetcher");
  let url = format!("http://{addr}/cookie");

  let initial = fetcher
    .cookie_header_value(&url)
    .expect("cookie_header_value should be Some for valid URL");
  assert!(
    initial.is_empty(),
    "expected no cookies initially, got {initial:?}"
  );
  assert!(fetcher.cookie_header_value("not a url").is_none());

  let res1 = fetcher.fetch(&url).expect("first fetch");
  assert_eq!(res1.bytes, b"first");

  let cookies = fetcher
    .cookie_header_value(&url)
    .expect("cookie_header_value should be Some for valid URL");
  assert!(
    cookies.contains("a=b"),
    "expected cookie_header_value to contain a=b, got {cookies:?}"
  );

  fetcher.store_cookie_from_document(&url, "c=d; Path=/");
  let cookies = fetcher
    .cookie_header_value(&url)
    .expect("cookie_header_value should be Some for valid URL");
  assert!(
    cookies.contains("a=b") && cookies.contains("c=d"),
    "expected cookie_header_value to contain a=b and c=d, got {cookies:?}"
  );

  let res2 = fetcher.fetch(&url).expect("second fetch");
  assert_eq!(res2.bytes, b"second");

  // Server saw two requests; ensure the second includes the cookie.
  let _first_cookie = cookie_rx
    .recv_timeout(Duration::from_secs(1))
    .expect("first http request capture");
  let second_cookie = cookie_rx
    .recv_timeout(Duration::from_secs(1))
    .expect("second http request capture")
    .unwrap_or_default();
  assert!(
    second_cookie.contains("a=b") && second_cookie.contains("c=d"),
    "expected second HTTP request to include Cookie: a=b and c=d, got {second_cookie:?}"
  );

  drop(fetcher);
  http_handle.join().unwrap();
  ipc_handle.join().unwrap();
}

#[test]
#[cfg(feature = "direct_network")]
fn ipc_fetcher_store_cookie_from_document_round_trips_into_cookie_header_value() {
  let _net_guard = net_test_lock();
  let Some(ipc_listener) = try_bind_localhost(
    "ipc_fetcher_store_cookie_from_document_round_trips_into_cookie_header_value",
  ) else {
    return;
  };
  let ipc_addr = ipc_listener.local_addr().unwrap();
  // store_cookie_from_document, cookie_header_value.
  let ipc_handle = spawn_network_process(ipc_listener, 2);

  let fetcher =
    IpcResourceFetcher::new_with_auth_token(ipc_addr.to_string(), TEST_AUTH_TOKEN).expect("connect ipc fetcher");
  let url = "http://127.0.0.1/".to_string();
  fetcher.store_cookie_from_document(&url, "c=d; Path=/");
  fetcher.store_cookie_from_document(&url, &format!("oversize={}; Path=/", "x".repeat(5000)));
  let cookies = fetcher
    .cookie_header_value(&url)
    .expect("cookie_header_value should be Some for valid URL");
  assert!(
    cookies.contains("c=d"),
    "expected cookie_header_value to contain c=d, got {cookies:?}"
  );
  assert!(
    !cookies.contains("oversize="),
    "expected oversize cookie to be ignored, got {cookies:?}"
  );

  drop(fetcher);
  ipc_handle.join().unwrap();
}

#[test]
fn ipc_fetcher_cookie_header_value_is_deterministic_when_remote_returns_none() {
  let _net_guard = net_test_lock();
  let Some(ipc_listener) = try_bind_localhost(
    "ipc_fetcher_cookie_header_value_is_deterministic_when_remote_returns_none",
  ) else {
    return;
  };
  let ipc_addr = ipc_listener.local_addr().unwrap();

  let ipc_handle = thread::spawn(move || {
    let (mut stream, _) = ipc_listener.accept().unwrap();
    stream
      .set_read_timeout(Some(Duration::from_secs(2)))
      .unwrap();
    stream
      .set_write_timeout(Some(Duration::from_secs(2)))
      .unwrap();

    // Auth handshake must precede any other IPC request.
    let hello_bytes = read_frame(&mut stream).expect("read ipc hello frame");
    let hello: IpcRequest = serde_json::from_slice(&hello_bytes).expect("decode ipc hello request");
    match hello {
      IpcRequest::Hello { token } => assert_eq!(token, TEST_AUTH_TOKEN, "unexpected IPC auth token"),
      other => panic!("expected IPC hello request, got {other:?}"),
    }
    let hello_ack = serde_json::to_vec(&IpcResponse::HelloAck).expect("encode ipc hello ack");
    write_frame(&mut stream, &hello_ack).expect("write ipc hello ack");

    let req_bytes = read_frame(&mut stream).expect("read ipc request frame");
    let env: BrowserToNetwork = serde_json::from_slice(&req_bytes).expect("decode ipc request");
    match env.request {
      IpcRequest::CookieHeaderValue { .. } => {}
      other => panic!("unexpected IPC request: {other:?}"),
    }
    let response = IpcResponse::MaybeString(IpcResult::Ok(None));
    let mut service = NetworkService::new(&mut stream);
    service
      .send_response(env.id, response)
      .expect("write ipc response");
  });

  let fetcher =
    IpcResourceFetcher::new_with_auth_token(ipc_addr.to_string(), TEST_AUTH_TOKEN).expect("connect ipc fetcher");
  let cookie = fetcher
    .cookie_header_value("http://example.com/")
    .expect("cookie_header_value should be Some for valid URL");
  assert!(cookie.is_empty(), "expected empty cookie string, got {cookie:?}");
  drop(fetcher);
  ipc_handle.join().unwrap();
}

#[test]
fn ipc_fetcher_store_cookie_from_document_oversize_is_not_sent_over_ipc() {
  let _net_guard = net_test_lock();
  let Some(ipc_listener) = try_bind_localhost(
    "ipc_fetcher_store_cookie_from_document_oversize_is_not_sent_over_ipc",
  ) else {
    return;
  };
  let ipc_addr = ipc_listener.local_addr().unwrap();

  let ipc_handle = thread::spawn(move || {
    let (mut stream, _) = ipc_listener.accept().unwrap();
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    // Auth handshake must precede any other IPC request.
    let hello_bytes = read_frame(&mut stream).expect("read ipc hello frame");
    let hello: IpcRequest = serde_json::from_slice(&hello_bytes).expect("decode ipc hello request");
    match hello {
      IpcRequest::Hello { token } => assert_eq!(token, TEST_AUTH_TOKEN, "unexpected IPC auth token"),
      other => panic!("expected IPC hello request, got {other:?}"),
    }
    let hello_ack = serde_json::to_vec(&IpcResponse::HelloAck).expect("encode ipc hello ack");
    write_frame(&mut stream, &hello_ack).expect("write ipc hello ack");

    // After the handshake, the oversize cookie should cause the client to send nothing; we should
    // observe EOF when the fetcher is dropped.
    let mut buf = [0u8; 1];
    match stream.read(&mut buf) {
      Ok(0) => {}
      Ok(n) => panic!("expected no IPC bytes after hello, got {n}"),
      Err(err)
        if err.kind() == io::ErrorKind::WouldBlock || err.kind() == io::ErrorKind::TimedOut =>
      {
        panic!("timed out waiting for IPC stream to close without sending bytes");
      }
      Err(err) => panic!("IPC read failed: {err}"),
    }
  });

  let fetcher =
    IpcResourceFetcher::new_with_auth_token(ipc_addr.to_string(), TEST_AUTH_TOKEN).expect("connect ipc fetcher");
  fetcher.store_cookie_from_document(
    "http://example.com/",
    &format!("oversize={}; Path=/", "x".repeat(5000)),
  );
  drop(fetcher);
  ipc_handle.join().unwrap();
}
