use crate::common::net::{net_test_lock, try_bind_localhost};
use base64::Engine as _;
use fastrender::resource::ipc_fetcher::{
  BrowserToNetwork, IpcFetchedResourceMeta, IpcRequest, IpcResponse, NetworkService, NetworkToBrowser,
};
use fastrender::resource::{FetchDestination, FetchRequest, HttpFetcher};
use fastrender::{IpcResourceFetcher, ResourceFetcher};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

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

#[test]
fn ipc_fetcher_chunked_large_body_round_trips() {
  let _net_guard = net_test_lock();
  let Some(listener) = try_bind_localhost("ipc_fetcher_chunked_large_body_round_trips") else {
    return;
  };
  let addr = listener.local_addr().unwrap();

  const BODY_LEN: usize = 2 * 1024 * 1024;
  let body = vec![b'a'; BODY_LEN];
  let http_handle = thread::spawn(move || {
    let (mut stream, _) = listener.accept().unwrap();
    // Drain request headers so the client proceeds.
    let mut tmp = [0u8; 1024];
    let _ = stream.read(&mut tmp);
    let response = format!(
      "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
      body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
    stream.write_all(&body).unwrap();
  });

  let ipc_listener = TcpListener::bind("127.0.0.1:0").unwrap();
  let ipc_addr = ipc_listener.local_addr().unwrap();
  let ipc_handle = thread::spawn(move || {
    let (mut stream, _) = ipc_listener.accept().unwrap();
    stream
      .set_read_timeout(Some(Duration::from_secs(5)))
      .unwrap();

    // Auth handshake must precede any other IPC request.
    let hello_bytes = read_frame(&mut stream).unwrap();
    let hello: IpcRequest = serde_json::from_slice(&hello_bytes).unwrap();
    match hello {
      IpcRequest::Hello { token } => assert_eq!(token, TEST_AUTH_TOKEN, "unexpected IPC auth token"),
      other => panic!("expected IPC hello request, got {other:?}"),
    }
    let hello_ack = serde_json::to_vec(&IpcResponse::HelloAck).unwrap();
    write_frame(&mut stream, &hello_ack).unwrap();

    let req_bytes = read_frame(&mut stream).unwrap();
    let env: BrowserToNetwork = serde_json::from_slice(&req_bytes).unwrap();

    let url = match &env.request {
      IpcRequest::Fetch { url } => url.clone(),
      other => panic!("unexpected ipc request: {other:?}"),
    };

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    let mut service = NetworkService::new(&mut stream);
    service
      .send_fetch_result(env.id, fetcher.fetch(&url))
      .unwrap();
  });

  let fetcher =
    IpcResourceFetcher::new_with_auth_token(ipc_addr.to_string(), TEST_AUTH_TOKEN).expect("connect ipc fetcher");
  let url = format!("http://{addr}/big");
  let res = fetcher.fetch(&url).expect("fetch large body");
  assert_eq!(res.bytes.len(), BODY_LEN);
  assert!(res.bytes.iter().all(|&b| b == b'a'));
  assert!(
    fetcher.last_response_was_chunked(),
    "expected IPC fetcher to observe chunked response"
  );

  http_handle.join().unwrap();
  ipc_handle.join().unwrap();
}

#[test]
fn ipc_fetcher_chunked_mismatched_total_len_is_protocol_error() {
  let _net_guard = net_test_lock();
  let ipc_listener = TcpListener::bind("127.0.0.1:0").unwrap();
  let ipc_addr = ipc_listener.local_addr().unwrap();

  let ipc_handle = thread::spawn(move || {
    let (mut stream, _) = ipc_listener.accept().unwrap();
    stream
      .set_read_timeout(Some(Duration::from_secs(5)))
      .unwrap();

    // Auth handshake must precede any other IPC request.
    let hello_bytes = read_frame(&mut stream).unwrap();
    let hello: IpcRequest = serde_json::from_slice(&hello_bytes).unwrap();
    match hello {
      IpcRequest::Hello { token } => assert_eq!(token, TEST_AUTH_TOKEN, "unexpected IPC auth token"),
      other => panic!("expected IPC hello request, got {other:?}"),
    }
    let hello_ack = serde_json::to_vec(&IpcResponse::HelloAck).unwrap();
    write_frame(&mut stream, &hello_ack).unwrap();

    let req_bytes = read_frame(&mut stream).unwrap();
    let env: BrowserToNetwork = serde_json::from_slice(&req_bytes).unwrap();

    // Send a malformed chunked response: total_len=10 but only 9 bytes delivered.
    let meta = IpcFetchedResourceMeta {
      content_type: Some("text/plain".to_string()),
      nosniff: false,
      content_encoding: None,
      status: Some(200),
      etag: None,
      last_modified: None,
      access_control_allow_origin: None,
      timing_allow_origin: None,
      vary: None,
      response_referrer_policy: None,
      access_control_allow_credentials: false,
      final_url: None,
      cache_policy: None,
      response_headers: None,
    };

    let mut service = NetworkService::new(&mut stream);
    // Bypass `send_fetch_ok` so we can send a mismatched total_len.
    service
      .send_message(&NetworkToBrowser::FetchStart {
        id: env.id,
        meta,
        total_len: 10,
      })
      .unwrap();
    let bytes_b64 = base64::engine::general_purpose::STANDARD.encode(b"123456789");
    service
      .send_message(&NetworkToBrowser::FetchBodyChunk {
        id: env.id,
        bytes_b64,
      })
      .unwrap();
    service
      .send_message(&NetworkToBrowser::FetchEnd { id: env.id })
      .unwrap();
  });

  let fetcher =
    IpcResourceFetcher::new_with_auth_token(ipc_addr.to_string(), TEST_AUTH_TOKEN).expect("connect ipc fetcher");
  let url = "http://example.invalid/proto".to_string();
  let req = FetchRequest::new(&url, FetchDestination::Fetch);
  let err = fetcher
    .fetch_with_request(req)
    .expect_err("expected protocol error");
  let msg = err.to_string();
  assert!(
    msg.contains("did not match total_len") || msg.contains("protocol error"),
    "unexpected error message: {msg}"
  );

  ipc_handle.join().unwrap();
}
