use fastrender::resource::ipc_fetcher::{IpcRequest, IpcResponse};
use fastrender::IpcResourceFetcher;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn write_frame(stream: &mut TcpStream, payload: &[u8]) {
  let len = (payload.len() as u32).to_le_bytes();
  stream.write_all(&len).unwrap();
  stream.write_all(payload).unwrap();
  stream.flush().unwrap();
}

fn read_frame(stream: &mut TcpStream) -> Vec<u8> {
  let mut len_buf = [0u8; 4];
  stream.read_exact(&mut len_buf).unwrap();
  let len = u32::from_le_bytes(len_buf) as usize;
  let mut buf = vec![0u8; len];
  stream.read_exact(&mut buf).unwrap();
  buf
}

#[test]
fn ipc_fetcher_rejects_wrong_auth_token() {
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  let addr = listener.local_addr().unwrap();
  let expected = "correct-token".to_string();

  let (tx, rx) = mpsc::channel::<String>();
  let server = thread::spawn(move || {
    let (mut stream, _) = listener.accept().unwrap();
    stream
      .set_read_timeout(Some(Duration::from_secs(2)))
      .unwrap();

    let hello_bytes = read_frame(&mut stream);
    let hello: IpcRequest = serde_json::from_slice(&hello_bytes).unwrap();
    match hello {
      IpcRequest::Hello { token } => {
        tx.send(token.clone()).unwrap();
        if token != expected {
          // Wrong token: close connection without sending a response.
          return;
        }
      }
      other => panic!("expected Hello request, got {other:?}"),
    }

    // If the token matched (should not in this test), send the ack so the client can proceed.
    let ack = serde_json::to_vec(&IpcResponse::HelloAck).unwrap();
    write_frame(&mut stream, &ack);
  });

  let err = match IpcResourceFetcher::new_with_auth_token(addr.to_string(), "wrong-token") {
    Ok(_) => panic!("expected wrong auth token to be rejected"),
    Err(err) => err,
  };
  let _ = err; // error message is not stable; assert by server observation instead.

  let observed = rx
    .recv_timeout(Duration::from_secs(1))
    .expect("server should observe client token");
  assert_eq!(observed, "wrong-token");

  server.join().unwrap();
}
