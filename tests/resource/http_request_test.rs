use crate::test_support;
use fastrender::resource::{FetchDestination, FetchRequest, HttpFetcher, HttpRequest};
use fastrender::resource::web_fetch::RequestRedirect;
use fastrender::ResourceFetcher;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use test_support::net::{net_test_lock, try_bind_localhost};

const MAX_WAIT: Duration = Duration::from_secs(3);

#[derive(Debug)]
struct CapturedRequest {
  method: String,
  path: String,
  headers: HashMap<String, String>,
  body: Vec<u8>,
}

fn header_end_and_content_len(buf: &[u8]) -> Option<(usize, usize)> {
  let end = buf.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
  let head = String::from_utf8_lossy(&buf[..end]);
  let mut content_len = 0usize;
  for line in head.split("\r\n").skip(1) {
    if line.is_empty() {
      break;
    }
    let Some((name, value)) = line.split_once(':') else {
      continue;
    };
    if name.trim().eq_ignore_ascii_case("content-length") {
      if let Ok(parsed) = value.trim().parse::<usize>() {
        content_len = parsed;
      }
    }
  }
  Some((end, content_len))
}

fn read_http_request_with_body(stream: &mut std::net::TcpStream) -> Vec<u8> {
  let mut buf = Vec::new();
  let mut tmp = [0u8; 1024];
  let start = Instant::now();
  while start.elapsed() < MAX_WAIT {
    match stream.read(&mut tmp) {
      Ok(0) => break,
      Ok(n) => buf.extend_from_slice(&tmp[..n]),
      Err(ref e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
        thread::sleep(Duration::from_millis(5));
      }
      Err(_) => break,
    }

    if let Some((head_end, content_len)) = header_end_and_content_len(&buf) {
      if buf.len() >= head_end + content_len {
        break;
      }
    }
  }
  buf
}

fn parse_request(raw: Vec<u8>) -> CapturedRequest {
  let (head_end, content_len) = header_end_and_content_len(&raw).unwrap_or((raw.len(), 0));
  let head = String::from_utf8_lossy(&raw[..head_end]);
  let mut lines = head.split("\r\n");
  let request_line = lines.next().unwrap_or_default();
  let mut parts = request_line.split_whitespace();
  let method = parts.next().unwrap_or_default().to_string();
  let path = parts.next().unwrap_or_default().to_string();
  let mut headers = HashMap::new();
  for line in lines {
    if line.is_empty() {
      break;
    }
    let Some((name, value)) = line.split_once(':') else {
      continue;
    };
    headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
  }
  let mut body = raw.get(head_end..).unwrap_or_default().to_vec();
  if body.len() > content_len {
    body.truncate(content_len);
  }
  CapturedRequest {
    method,
    path,
    headers,
    body,
  }
}

#[test]
fn http_request_post_sends_method_headers_and_body() {
  let _net_guard = net_test_lock();
  let Some(listener) = try_bind_localhost("http_request_post_sends_method_headers_and_body") else {
    return;
  };
  let addr = listener.local_addr().unwrap();

  let (tx, rx) = mpsc::channel::<CapturedRequest>();
  let handle = thread::spawn(move || {
    let (mut stream, _) = listener.accept().unwrap();
    stream.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    let raw = read_http_request_with_body(&mut stream);
    tx.send(parse_request(raw)).unwrap();
    let body = b"ok";
    let response = format!(
      "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
      body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
  });

  let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
  let url = format!("http://{addr}/submit");
  let fetch = FetchRequest::new(&url, FetchDestination::Fetch);
  let user_headers = vec![
    ("X-Test".to_string(), "hello".to_string()),
    // Override the destination-derived default `Accept` so we can assert header precedence.
    ("Accept".to_string(), "text/plain".to_string()),
  ];
  let body = b"payload";
  let req = HttpRequest {
    fetch,
    method: "POST",
    redirect: RequestRedirect::Follow,
    headers: &user_headers,
    body: Some(body),
  };
  let res = fetcher.fetch_http_request(req).expect("fetch_http_request");
  assert_eq!(res.bytes, b"ok");

  let captured = rx.recv_timeout(Duration::from_secs(1)).expect("captured request");
  assert_eq!(captured.method, "POST");
  assert_eq!(captured.path, "/submit");
  assert_eq!(captured.headers.get("x-test").map(String::as_str), Some("hello"));
  assert_eq!(
    captured.headers.get("accept").map(String::as_str),
    Some("text/plain")
  );
  assert_eq!(captured.body, body);

  handle.join().unwrap();
}

#[test]
fn http_request_head_returns_empty_body() {
  let _net_guard = net_test_lock();
  let Some(listener) = try_bind_localhost("http_request_head_returns_empty_body") else {
    return;
  };
  let addr = listener.local_addr().unwrap();

  let (tx, rx) = mpsc::channel::<CapturedRequest>();
  let handle = thread::spawn(move || {
    let (mut stream, _) = listener.accept().unwrap();
    stream.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    let raw = read_http_request_with_body(&mut stream);
    tx.send(parse_request(raw)).unwrap();
    // Intentionally send a body even though HEAD responses should not include one; the fetcher must
    // still return an empty body.
    let body = b"abc";
    let response = format!(
      "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
      body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
  });

  let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
  let url = format!("http://{addr}/head");
  let fetch = FetchRequest::new(&url, FetchDestination::Fetch);
  let req = HttpRequest::new(fetch, "HEAD");
  let res = fetcher.fetch_http_request(req).expect("HEAD request");
  assert_eq!(res.status, Some(200));
  assert!(res.bytes.is_empty(), "expected empty body for HEAD");

  let captured = rx.recv_timeout(Duration::from_secs(1)).expect("captured request");
  assert_eq!(captured.method, "HEAD");
  assert_eq!(captured.path, "/head");

  handle.join().unwrap();
}

#[test]
fn http_request_redirect_updates_final_url_and_downgrades_post_to_get() {
  let _net_guard = net_test_lock();
  let Some(listener) = try_bind_localhost("http_request_redirect_updates_final_url_and_downgrades_post_to_get") else {
    return;
  };
  let addr = listener.local_addr().unwrap();

  let (tx, rx) = mpsc::channel::<CapturedRequest>();
  let handle = thread::spawn(move || {
    // Redirect response.
    let (mut stream, _) = listener.accept().unwrap();
    stream.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    let raw = read_http_request_with_body(&mut stream);
    tx.send(parse_request(raw)).unwrap();
    let response = "HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    stream.write_all(response.as_bytes()).unwrap();

    // Final response.
    let (mut stream, _) = listener.accept().unwrap();
    stream.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    let raw = read_http_request_with_body(&mut stream);
    tx.send(parse_request(raw)).unwrap();
    let body = b"ok";
    let response = format!(
      "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
      body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
  });

  let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
  let url = format!("http://{addr}/redirect");
  let fetch = FetchRequest::new(&url, FetchDestination::Fetch);
  let req = HttpRequest {
    fetch,
    method: "POST",
    redirect: RequestRedirect::Follow,
    headers: &[],
    body: Some(b"hello"),
  };
  let res = fetcher.fetch_http_request(req).expect("redirect fetch");
  assert_eq!(res.bytes, b"ok");
  let expected_final = format!("http://{addr}/final");
  assert_eq!(res.final_url.as_deref(), Some(expected_final.as_str()));

  let first = rx.recv_timeout(Duration::from_secs(1)).expect("first request");
  assert_eq!(first.method, "POST");
  assert_eq!(first.path, "/redirect");
  assert_eq!(first.body, b"hello");

  let second = rx.recv_timeout(Duration::from_secs(1)).expect("second request");
  assert_eq!(second.method, "GET");
  assert_eq!(second.path, "/final");
  assert!(second.body.is_empty(), "redirected request should drop body");

  handle.join().unwrap();
}
