use crate::common::net::{net_test_lock, try_bind_localhost};
use fastrender::resource::ipc_fetcher::{IpcRequest, IpcResponse, IPC_MAX_OUTBOUND_FRAME_BYTES};
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
fn ipc_fetcher_rejects_oversized_response_frame() {
  let _net_guard = net_test_lock();
  let Some(listener) = try_bind_localhost("ipc_fetcher_rejects_oversized_response_frame") else {
    return;
  };
  let ipc_addr = listener.local_addr().unwrap();

  let server = thread::spawn(move || {
    let (mut stream, _) = listener.accept().expect("accept ipc client");
    stream
      .set_read_timeout(Some(Duration::from_secs(5)))
      .unwrap();
    stream
      .set_write_timeout(Some(Duration::from_secs(5)))
      .unwrap();

    // Hello/ack handshake.
    let hello_bytes = read_frame(&mut stream).expect("read hello frame");
    let hello: IpcRequest = serde_json::from_slice(&hello_bytes).expect("decode hello request");
    match hello {
      IpcRequest::Hello { token } => assert_eq!(token, TEST_AUTH_TOKEN, "unexpected IPC auth token"),
      other => panic!("expected IPC hello request, got {other:?}"),
    }
    let ack = serde_json::to_vec(&IpcResponse::HelloAck).expect("encode hello ack");
    write_frame(&mut stream, &ack).expect("write hello ack");

    // Read a single request frame, then reply with an oversized response header.
    let _req = read_frame(&mut stream).expect("read request frame");

    let oversized_len: u32 = (IPC_MAX_OUTBOUND_FRAME_BYTES + 1)
      .try_into()
      .expect("max outbound frame bytes should fit in u32");
    stream
      .write_all(&oversized_len.to_le_bytes())
      .expect("write oversized length prefix");
    stream.flush().expect("flush oversized prefix");
  });

  let fetcher =
    IpcResourceFetcher::new_with_auth_token(ipc_addr.to_string(), TEST_AUTH_TOKEN).expect("connect fetcher");
  let err = fetcher
    .fetch("http://example.test/oversize")
    .expect_err("expected oversized frame to be rejected");
  let msg = err.to_string();
  assert!(
    msg.contains("frame too large"),
    "unexpected error message: {msg}"
  );

  drop(fetcher);
  server.join().unwrap();
}

