use fastrender::multiprocess::network_fetch::{NetworkService, NetworkToBrowser};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[test]
fn network_fetch_cancel_suppresses_completion_and_future_fetches_still_work() {
  let listener = match TcpListener::bind("127.0.0.1:0") {
    Ok(listener) => listener,
    Err(err) => {
      eprintln!("skipping test: failed to bind localhost: {err}");
      return;
    }
  };
  let addr = listener.local_addr().expect("local addr");

  let received_pair: Arc<(Mutex<bool>, Condvar)> = Arc::new((Mutex::new(false), Condvar::new()));
  let release_pair: Arc<(Mutex<bool>, Condvar)> = Arc::new((Mutex::new(false), Condvar::new()));

  let server_join = std::thread::spawn({
    let received_pair = Arc::clone(&received_pair);
    let release_pair = Arc::clone(&release_pair);
    move || {
      // Request 1: wait until released before responding.
      let (mut stream, _) = listener.accept().expect("accept 1");
      stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
      let _headers = read_http_request_headers(&mut stream);

      // Signal that the request is in-flight (client is now blocked waiting for response).
      {
        let (lock, cv) = &*received_pair;
        let mut received = lock.lock().unwrap();
        *received = true;
        cv.notify_all();
      }

      // Block the response until the test cancels.
      {
        let (lock, cv) = &*release_pair;
        let mut released = lock.lock().unwrap();
        while !*released {
          let (guard, _timeout) = cv.wait_timeout(released, Duration::from_secs(1)).unwrap();
          released = guard;
        }
      }

      write_http_ok(&mut stream, b"first");
      drop(stream);

      // Request 2: respond immediately.
      let (mut stream, _) = listener.accept().expect("accept 2");
      stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
      let _headers = read_http_request_headers(&mut stream);
      write_http_ok(&mut stream, b"second");
    }
  });

  let (client, _service) = NetworkService::spawn();

  let url1 = format!("http://{addr}/slow");
  let id1 = client.fetch(url1);

  // Wait until the server has definitely received the request (so cancellation is meaningful and
  // deterministic).
  {
    let (lock, cv) = &*received_pair;
    let mut received = lock.lock().unwrap();
    let start = Instant::now();
    while !*received {
      let remaining = Duration::from_secs(5).saturating_sub(start.elapsed());
      assert!(
        !remaining.is_zero(),
        "timed out waiting for server to observe first request"
      );
      let (guard, _timeout) = cv.wait_timeout(received, remaining).unwrap();
      received = guard;
    }
  }

  client.cancel(id1);

  // Allow the server to finish the response (network thread should still not deliver it).
  {
    let (lock, cv) = &*release_pair;
    let mut released = lock.lock().unwrap();
    *released = true;
    cv.notify_all();
  }

  // Observe either an explicit cancellation message or no message at all, but never a success or
  // error completion for the cancelled request.
  let mut saw_cancelled = false;
  assert_no_completion_for_id_until(
    &client,
    id1,
    &mut saw_cancelled,
    Instant::now() + Duration::from_millis(500),
  );

  // Subsequent requests should still work.
  let url2 = format!("http://{addr}/fast");
  let id2 = client.fetch(url2);
  let mut got_second = None;
  let deadline = Instant::now() + Duration::from_secs(2);
  while Instant::now() < deadline {
    match client.recv_timeout(Duration::from_millis(100)) {
      Some(NetworkToBrowser::FetchOk { id, response }) if id == id2 => {
        got_second = Some(response);
        break;
      }
      Some(NetworkToBrowser::FetchErr { id, error }) if id == id2 => {
        panic!("second fetch unexpectedly failed: {error}");
      }
      Some(NetworkToBrowser::FetchOk { id, .. }) if id == id1 => {
        panic!("cancelled request received FetchOk while waiting for second response");
      }
      Some(NetworkToBrowser::FetchErr { id, .. }) if id == id1 => {
        panic!("cancelled request received FetchErr while waiting for second response");
      }
      Some(NetworkToBrowser::FetchCancelled { id }) if id == id1 => {
        saw_cancelled = true;
      }
      Some(_other) => {}
      None => {}
    }
  }

  let got_second = got_second.expect("timed out waiting for second fetch response");
  assert_eq!(got_second.status, 200);
  assert_eq!(got_second.body, b"second".to_vec());

  // Ensure no late completion sneaks through after the second response is delivered.
  assert_no_completion_for_id_until(
    &client,
    id1,
    &mut saw_cancelled,
    Instant::now() + Duration::from_millis(200),
  );

  // The cancelled request should either have been explicitly cancelled or silently dropped. The
  // test accepts both behaviors (per spec requirement).
  let _ = saw_cancelled;

  server_join.join().expect("server thread");
}

fn assert_no_completion_for_id_until(
  client: &fastrender::multiprocess::network_fetch::NetworkClient,
  cancelled_id: u64,
  saw_cancelled: &mut bool,
  deadline: Instant,
) {
  while Instant::now() < deadline {
    match client.recv_timeout(Duration::from_millis(25)) {
      Some(NetworkToBrowser::FetchOk { id, .. }) if id == cancelled_id => {
        panic!("cancelled request received FetchOk");
      }
      Some(NetworkToBrowser::FetchErr { id, .. }) if id == cancelled_id => {
        panic!("cancelled request received FetchErr");
      }
      Some(NetworkToBrowser::FetchCancelled { id }) if id == cancelled_id => {
        *saw_cancelled = true;
      }
      Some(_other) => {}
      None => {}
    }
  }
}

fn read_http_request_headers(stream: &mut std::net::TcpStream) -> String {
  let mut buf = Vec::new();
  let mut tmp = [0u8; 1024];
  while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
    let n = stream.read(&mut tmp).expect("read request");
    if n == 0 {
      break;
    }
    buf.extend_from_slice(&tmp[..n]);
    if buf.len() > 64 * 1024 {
      break;
    }
  }
  String::from_utf8_lossy(&buf).to_string()
}

fn write_http_ok(stream: &mut std::net::TcpStream, body: &[u8]) {
  let response = format!(
    concat!(
      "HTTP/1.1 200 OK\r\n",
      "Content-Type: text/plain\r\n",
      "Content-Length: {}\r\n",
      "Connection: close\r\n",
      "\r\n"
    ),
    body.len()
  );
  stream.write_all(response.as_bytes()).unwrap();
  stream.write_all(body).unwrap();
  let _ = stream.flush();
}
