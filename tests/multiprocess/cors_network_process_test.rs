use crate::common::net::{net_test_lock, try_bind_localhost};
use fastrender::resource::web_fetch::RequestRedirect;
use fastrender::resource::{
  origin_from_url, FetchCredentialsMode, FetchDestination, FetchRequest, HttpRequest,
};
use fastrender::{IpcResourceFetcher, ResourceFetcher};
use std::env;
use std::io::{self, BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_WAIT: Duration = Duration::from_secs(3);
const TEST_AUTH_TOKEN: &str = "fastrender-network-process-test-token";

fn network_process_exe() -> PathBuf {
  PathBuf::from(
    env::var_os("CARGO_BIN_EXE_network_process")
      .expect("CARGO_BIN_EXE_network_process missing; network_process binary was not built"),
  )
}

fn spawn_network_process() -> (Child, String) {
  let exe = network_process_exe();
  let mut cmd = Command::new(exe);
  cmd.arg("--bind").arg("127.0.0.1:0");
  cmd.arg("--auth-token").arg(TEST_AUTH_TOKEN);
  cmd.stdin(Stdio::null());
  cmd.stdout(Stdio::piped());
  cmd.stderr(Stdio::null());

  let mut child = cmd.spawn().expect("spawn network_process binary");
  let stdout = child.stdout.take().expect("capture network_process stdout");

  let (tx, rx) = mpsc::channel::<io::Result<String>>();
  thread::spawn(move || {
    let mut reader = io::BufReader::new(stdout);
    let mut line = String::new();
    let res = reader.read_line(&mut line).map(|_| line);
    let _ = tx.send(res);
  });

  let line = rx
    .recv_timeout(MAX_WAIT)
    .expect("timed out waiting for network_process handshake")
    .expect("read network_process handshake");
  let addr = line.trim().to_string();
  assert!(!addr.is_empty(), "network_process reported empty address");

  (child, addr)
}

fn read_http_headers(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
  let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
  let mut buf = [0u8; 1024];
  let mut data = Vec::new();
  loop {
    match stream.read(&mut buf) {
      Ok(0) => break,
      Ok(n) => {
        data.extend_from_slice(&buf[..n]);
        if data.windows(4).any(|w| w == b"\r\n\r\n") {
          break;
        }
      }
      Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
      Err(err) => return Err(err),
    }
  }
  Ok(data)
}

fn spawn_http_server(listener: TcpListener, cors_headers: &'static str) -> thread::JoinHandle<()> {
  thread::spawn(move || {
    let _ = listener.set_nonblocking(true);
    let start = Instant::now();
    while start.elapsed() < MAX_WAIT {
      match listener.accept() {
        Ok((mut stream, _)) => {
          let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
          let raw = read_http_headers(&mut stream).unwrap_or_default();
          let request = String::from_utf8_lossy(&raw).to_ascii_lowercase();
          assert!(
            request.contains("sec-fetch-mode: cors"),
            "expected CORS-mode request, got: {request}"
          );
          assert!(
            request.contains("origin: https://client.example"),
            "expected Origin header on CORS-mode request, got: {request}"
          );

          let body = b"secret";
          let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n{cors_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
          );
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
#[cfg(feature = "direct_network")]
fn cors_mode_fetch_blocked_in_network_process_when_missing_acao() {
  let _net_guard = net_test_lock();

  let Some(listener) =
    try_bind_localhost("cors_mode_fetch_blocked_in_network_process_when_missing_acao")
  else {
    return;
  };
  let addr = listener.local_addr().unwrap();
  let http_handle = spawn_http_server(listener, "");

  let (mut child, ipc_addr) = spawn_network_process();

  let fetcher = IpcResourceFetcher::new_with_auth_token(ipc_addr, TEST_AUTH_TOKEN)
    .expect("connect ipc fetcher");
  let url = format!("http://{addr}/secret");
  let client_origin = origin_from_url("https://client.example/").expect("client origin");
  let req = FetchRequest::new(&url, FetchDestination::Fetch).with_client_origin(&client_origin);

  let err = fetcher
    .fetch_with_request(req)
    .expect_err("expected network process to enforce CORS");
  assert!(
    err.to_string().contains("blocked by CORS"),
    "unexpected error message: {err}"
  );

  let _ = child.kill();
  let _ = child.wait();
  http_handle.join().unwrap();
}

#[test]
#[cfg(feature = "direct_network")]
fn cors_mode_fetch_allowed_in_network_process_with_matching_acao_and_credentials() {
  let _net_guard = net_test_lock();

  let Some(listener) = try_bind_localhost(
    "cors_mode_fetch_allowed_in_network_process_with_matching_acao_and_credentials",
  ) else {
    return;
  };
  let addr = listener.local_addr().unwrap();
  let http_handle = spawn_http_server(
    listener,
    "Access-Control-Allow-Origin: https://client.example\r\nAccess-Control-Allow-Credentials: true\r\n",
  );

  let (mut child, ipc_addr) = spawn_network_process();

  let fetcher = IpcResourceFetcher::new_with_auth_token(ipc_addr, TEST_AUTH_TOKEN)
    .expect("connect ipc fetcher");
  let url = format!("http://{addr}/secret");
  let client_origin = origin_from_url("https://client.example/").expect("client origin");
  let req = FetchRequest::new(&url, FetchDestination::Fetch)
    .with_client_origin(&client_origin)
    .with_credentials_mode(FetchCredentialsMode::Include);

  let res = fetcher
    .fetch_with_request(req)
    .expect("expected CORS response to be allowed");
  assert_eq!(res.bytes, b"secret");

  let _ = child.kill();
  let _ = child.wait();
  http_handle.join().unwrap();
}

#[test]
#[cfg(feature = "direct_network")]
fn cors_mode_http_request_blocked_in_network_process_and_ignores_malicious_origin_header() {
  let _net_guard = net_test_lock();

  let Some(listener) = try_bind_localhost(
    "cors_mode_http_request_blocked_in_network_process_and_ignores_malicious_origin_header",
  ) else {
    return;
  };
  let addr = listener.local_addr().unwrap();
  let http_handle = spawn_http_server(listener, "");

  let (mut child, ipc_addr) = spawn_network_process();

  let fetcher = IpcResourceFetcher::new_with_auth_token(ipc_addr, TEST_AUTH_TOKEN)
    .expect("connect ipc fetcher");
  let url = format!("http://{addr}/secret");
  let client_origin = origin_from_url("https://client.example/").expect("client origin");
  let fetch = FetchRequest::new(&url, FetchDestination::Fetch).with_client_origin(&client_origin);

  // Simulate a malicious renderer trying to spoof the Origin request header directly. The HTTP
  // fetch layer must drop user-provided Origin headers and instead compute them from the
  // client-origin context.
  let user_headers = vec![("Origin".to_string(), "https://evil.example".to_string())];
  let req = HttpRequest {
    fetch,
    method: "GET",
    redirect: RequestRedirect::Follow,
    headers: &user_headers,
    body: None,
  };

  let err = fetcher
    .fetch_http_request(req)
    .expect_err("expected network process to enforce CORS");
  assert!(
    err.to_string().contains("blocked by CORS"),
    "unexpected error message: {err}"
  );

  let _ = child.kill();
  let _ = child.wait();
  http_handle.join().unwrap();
}

#[test]
#[cfg(feature = "direct_network")]
fn credentialed_cors_mode_fetch_requires_allow_credentials_in_network_process() {
  let _net_guard = net_test_lock();

  let Some(listener) = try_bind_localhost(
    "credentialed_cors_mode_fetch_requires_allow_credentials_in_network_process",
  ) else {
    return;
  };
  let addr = listener.local_addr().unwrap();
  let http_handle = spawn_http_server(
    listener,
    // Matching ACAO but no Access-Control-Allow-Credentials.
    "Access-Control-Allow-Origin: https://client.example\r\n",
  );

  let (mut child, ipc_addr) = spawn_network_process();

  let fetcher = IpcResourceFetcher::new_with_auth_token(ipc_addr, TEST_AUTH_TOKEN)
    .expect("connect ipc fetcher");
  let url = format!("http://{addr}/secret");
  let client_origin = origin_from_url("https://client.example/").expect("client origin");
  let req = FetchRequest::new(&url, FetchDestination::Fetch)
    .with_client_origin(&client_origin)
    .with_credentials_mode(FetchCredentialsMode::Include);

  let err = fetcher
    .fetch_with_request(req)
    .expect_err("expected credentialed CORS request to require ACAC");
  assert!(
    err.to_string().contains("Access-Control-Allow-Credentials"),
    "unexpected error message: {err}"
  );

  let _ = child.kill();
  let _ = child.wait();
  http_handle.join().unwrap();
}
