use crate::common::net::{net_test_lock, try_bind_localhost};
use fastrender::resource::ipc_fetcher::{
  validate_ipc_request, BrowserToNetwork, IpcRequest, IpcResponse, NetworkService,
};
use fastrender::resource::{FetchDestination, FetchRequest, HttpFetcher};
use fastrender::{IpcResourceFetcher, ResourceFetcher};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
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
fn ipc_fetcher_fetch_partial_with_request_truncates() {
  let _net_guard = net_test_lock();
  let Some(listener) = try_bind_localhost("ipc_fetcher_fetch_partial_with_request_truncates")
  else {
    return;
  };
  let addr = listener.local_addr().unwrap();

  let body = b"abcdefghijklmnopqrstuvwxyz".to_vec();
  let http_handle = thread::spawn(move || {
    let (mut stream, _) = listener.accept().unwrap();
    let mut tmp = [0u8; 1024];
    // Drain request headers quickly so the client proceeds.
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
  let (ipc_tx, ipc_rx) = mpsc::channel::<IpcRequest>();
  let ipc_handle = thread::spawn(move || {
    let (mut stream, _) = ipc_listener.accept().unwrap();
    stream
      .set_read_timeout(Some(Duration::from_secs(5)))
      .unwrap();

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
    validate_ipc_request(&env.request).unwrap();
    ipc_tx.send(env.request.clone()).unwrap();

    let fetcher = HttpFetcher::new().with_timeout(Duration::from_secs(2));
    match env.request {
      IpcRequest::FetchPartialWithRequest { req, max_bytes } => {
        let fetch_req = req.as_fetch_request();
        let max_bytes = usize::try_from(max_bytes).unwrap_or(usize::MAX);
        let mut service = NetworkService::new(&mut stream);
        service
          .send_fetch_result(
            env.id,
            fetcher.fetch_partial_with_request(fetch_req, max_bytes),
          )
          .unwrap();
      }
      other => panic!("unexpected ipc request: {other:?}"),
    }
  });

  let fetcher =
    IpcResourceFetcher::new_with_auth_token(ipc_addr.to_string(), TEST_AUTH_TOKEN).expect("connect ipc fetcher");
  let url = format!("http://{addr}/partial");
  let req = FetchRequest::new(&url, FetchDestination::Fetch);
  let res = fetcher
    .fetch_partial_with_request(req, 3)
    .expect("fetch_partial_with_request");
  assert_eq!(res.bytes, b"abc");

  let captured = ipc_rx
    .recv_timeout(Duration::from_secs(1))
    .expect("captured ipc request");
  match captured {
    IpcRequest::FetchPartialWithRequest { max_bytes, .. } => {
      assert_eq!(max_bytes, 3);
    }
    other => panic!("unexpected ipc request: {other:?}"),
  }

  http_handle.join().unwrap();
  ipc_handle.join().unwrap();
}
