use crate::common::net::{net_test_lock, try_bind_localhost};
use fastrender::resource::ipc_fetcher::{IpcRequest, IpcResponse, IpcResult};
use fastrender::resource::web_fetch::RequestRedirect;
use fastrender::resource::{origin_from_url, FetchDestination, FetchRequest, HttpFetcher, HttpRequest, ReferrerPolicy};
use fastrender::{IpcResourceFetcher, ResourceFetcher};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

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

fn read_http_request_with_body(stream: &mut TcpStream) -> Vec<u8> {
  let mut buf = Vec::new();
  let mut tmp = [0u8; 1024];
  let start = Instant::now();
  while start.elapsed() < MAX_WAIT {
    match stream.read(&mut tmp) {
      Ok(0) => break,
      Ok(n) => buf.extend_from_slice(&tmp[..n]),
      Err(ref e)
        if matches!(
          e.kind(),
          io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ) =>
      {
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

fn spawn_ipc_server(listener: TcpListener, request_tx: mpsc::Sender<IpcRequest>) -> thread::JoinHandle<()> {
  thread::spawn(move || {
    let (mut stream, _) = listener.accept().unwrap();
    stream
      .set_read_timeout(Some(Duration::from_secs(5)))
      .unwrap();

    let req_bytes = read_frame(&mut stream).unwrap();
    let req: IpcRequest = serde_json::from_slice(&req_bytes).unwrap();
    request_tx.send(req.clone()).unwrap();

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let response = match req {
      IpcRequest::FetchHttpRequest { req } => {
        let body = req.decode_body().unwrap();
        let fetch = req.fetch.as_fetch_request();
        let http_req = HttpRequest {
          fetch,
          method: &req.method,
          redirect: req.redirect,
          headers: &req.headers,
          body: body.as_deref(),
        };
        match fetcher.fetch_http_request(http_req) {
          Ok(res) => IpcResponse::Fetched(IpcResult::Ok(res.into())),
          Err(err) => IpcResponse::Fetched(IpcResult::Err(err.into())),
        }
      }
      other => panic!("unexpected ipc request: {other:?}"),
    };

    let out = serde_json::to_vec(&response).unwrap();
    write_frame(&mut stream, &out).unwrap();
  })
}

#[test]
fn ipc_fetcher_http_request_post_sends_method_headers_and_body() {
  let _net_guard = net_test_lock();
  let Some(listener) =
    try_bind_localhost("ipc_fetcher_http_request_post_sends_method_headers_and_body")
  else {
    return;
  };
  let addr = listener.local_addr().unwrap();

  let (http_tx, http_rx) = mpsc::channel::<CapturedRequest>();
  let http_handle = thread::spawn(move || {
    let (mut stream, _) = listener.accept().unwrap();
    stream
      .set_read_timeout(Some(Duration::from_millis(500)))
      .unwrap();
    let raw = read_http_request_with_body(&mut stream);
    http_tx.send(parse_request(raw)).unwrap();
    let body = b"ok";
    let response = format!(
      "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
      body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
  });

  let ipc_listener = TcpListener::bind("127.0.0.1:0").unwrap();
  let ipc_addr = ipc_listener.local_addr().unwrap();
  let (ipc_tx, ipc_rx) = mpsc::channel::<IpcRequest>();
  let ipc_handle = spawn_ipc_server(ipc_listener, ipc_tx);

  let fetcher = IpcResourceFetcher::new(ipc_addr.to_string()).expect("connect ipc fetcher");
  let url = format!("http://{addr}/submit");
  // Use a same-origin client origin so we don't trigger CORS enforcement in this basic request test.
  let origin = origin_from_url(&url).unwrap();
  let referrer_url = format!("http://{addr}/referrer");
  let fetch = FetchRequest::new(&url, FetchDestination::Fetch)
    .with_referrer_url(&referrer_url)
    .with_client_origin(&origin)
    .with_referrer_policy(ReferrerPolicy::UnsafeUrl)
    .with_credentials_mode(fastrender::resource::FetchCredentialsMode::Include);
  let user_headers = vec![
    ("X-Test".to_string(), "hello".to_string()),
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

  let captured_http = http_rx
    .recv_timeout(Duration::from_secs(1))
    .expect("captured http request");
  assert_eq!(captured_http.method, "POST");
  assert_eq!(captured_http.path, "/submit");
  assert_eq!(
    captured_http.headers.get("x-test").map(String::as_str),
    Some("hello")
  );
  assert_eq!(
    captured_http.headers.get("accept").map(String::as_str),
    Some("text/plain")
  );
  assert_eq!(captured_http.body, body);

  let captured_ipc = ipc_rx
    .recv_timeout(Duration::from_secs(1))
    .expect("captured ipc request");
  match captured_ipc {
    IpcRequest::FetchHttpRequest { req } => {
      assert_eq!(req.fetch.destination, FetchDestination::Fetch);
      assert_eq!(
        req.fetch.referrer_url.as_deref(),
        Some(referrer_url.as_str())
      );
      assert_eq!(req.fetch.client_origin.as_ref(), Some(&origin));
      assert_eq!(
        req.fetch.credentials_mode,
        fastrender::resource::FetchCredentialsMode::Include
      );
      assert_eq!(req.fetch.referrer_policy, ReferrerPolicy::UnsafeUrl);
      assert_eq!(req.method, "POST");
      assert_eq!(
        req.headers
          .iter()
          .find(|(k, _)| k.eq_ignore_ascii_case("x-test"))
          .map(|(_, v)| v.as_str()),
        Some("hello")
      );
      let decoded = req.decode_body().unwrap().unwrap();
      assert_eq!(decoded, body);
    }
    other => panic!("unexpected ipc request: {other:?}"),
  }

  http_handle.join().unwrap();
  ipc_handle.join().unwrap();
}

#[test]
fn ipc_fetcher_http_request_redirect_updates_final_url() {
  let _net_guard = net_test_lock();
  let Some(listener) = try_bind_localhost("ipc_fetcher_http_request_redirect_updates_final_url") else {
    return;
  };
  let addr = listener.local_addr().unwrap();

  let (tx, rx) = mpsc::channel::<CapturedRequest>();
  let http_handle = thread::spawn(move || {
    // Redirect response.
    let (mut stream, _) = listener.accept().unwrap();
    stream
      .set_read_timeout(Some(Duration::from_millis(500)))
      .unwrap();
    let raw = read_http_request_with_body(&mut stream);
    tx.send(parse_request(raw)).unwrap();
    let response =
      "HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    stream.write_all(response.as_bytes()).unwrap();

    // Final response.
    let (mut stream, _) = listener.accept().unwrap();
    stream
      .set_read_timeout(Some(Duration::from_millis(500)))
      .unwrap();
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

  let ipc_listener = TcpListener::bind("127.0.0.1:0").unwrap();
  let ipc_addr = ipc_listener.local_addr().unwrap();
  let (ipc_tx, _ipc_rx) = mpsc::channel::<IpcRequest>();
  let ipc_handle = spawn_ipc_server(ipc_listener, ipc_tx);

  let fetcher = IpcResourceFetcher::new(ipc_addr.to_string()).expect("connect ipc fetcher");
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

  let first = rx
    .recv_timeout(Duration::from_secs(1))
    .expect("first request");
  assert_eq!(first.method, "POST");
  assert_eq!(first.path, "/redirect");
  assert_eq!(first.body, b"hello");

  let second = rx
    .recv_timeout(Duration::from_secs(1))
    .expect("second request");
  assert_eq!(second.method, "GET");
  assert_eq!(second.path, "/final");
  assert!(
    second.body.is_empty(),
    "redirected request should drop body"
  );

  http_handle.join().unwrap();
  ipc_handle.join().unwrap();
}
