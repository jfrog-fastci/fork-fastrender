#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tempfile::tempdir;
use url::Url;

// Navigation + rendering on CI can take a few seconds when tests run in parallel; keep this
// generous to avoid flakes.
const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug)]
struct CapturedHttpRequest {
  method: String,
  path: String,
  headers: Vec<(String, String)>,
  body: Vec<u8>,
}

fn spawn_file_upload_server() -> (
  String,
  mpsc::Receiver<CapturedHttpRequest>,
  thread::JoinHandle<()>,
) {
  let listener = TcpListener::bind("127.0.0.1:0").expect("bind localhost");
  let addr = listener.local_addr().expect("local addr");
  let base = format!("http://{addr}");
  let page_url = format!("{base}/page.html");
  let (tx, rx) = mpsc::channel::<CapturedHttpRequest>();

  let join = thread::spawn(move || {
    for stream in listener.incoming() {
      let mut stream = match stream {
        Ok(stream) => stream,
        Err(_) => continue,
      };

      let (method, path, headers, body) = {
        let mut reader = BufReader::new(&mut stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
          continue;
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();

        let mut headers = Vec::new();
        let mut content_length: usize = 0;
        loop {
          let mut line = String::new();
          if reader.read_line(&mut line).is_err() {
            break;
          }
          let line = line.trim_end_matches(['\r', '\n']);
          if line.is_empty() {
            break;
          }
          if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_string();
            let value = value.trim().to_string();
            if name.eq_ignore_ascii_case("content-length") {
              content_length = value.parse::<usize>().unwrap_or(0);
            }
            headers.push((name, value));
          }
        }

        let mut body = vec![0u8; content_length];
        if content_length > 0 {
          let _ = reader.read_exact(&mut body);
        }

        (method, path, headers, body)
      };

      let (status_line, response_body) =
        if method.eq_ignore_ascii_case("GET") && path == "/page.html" {
          (
            "HTTP/1.1 200 OK",
            r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      #f { position: absolute; left: 0; top: 0; width: 240px; height: 40px; }
      #submit { position: absolute; left: 0; top: 60px; width: 120px; height: 40px; }
    </style>
  </head>
  <body>
    <form action="/result" method="post" enctype="multipart/form-data">
      <input id="f" type="file" multiple name="up">
      <input id="submit" type="submit" value="Go">
    </form>
  </body>
</html>
"#,
          )
        } else if method.eq_ignore_ascii_case("POST") && path == "/result" {
          let _ = tx.send(CapturedHttpRequest {
            method: method.clone(),
            path: path.clone(),
            headers: headers.clone(),
            body: body.clone(),
          });
          (
            "HTTP/1.1 200 OK",
            "<!doctype html><html><body>ok</body></html>",
          )
        } else {
          (
            "HTTP/1.1 404 Not Found",
            "<!doctype html><html><body>not found</body></html>",
          )
        };

      let response_body_bytes = response_body.as_bytes();
      let response = format!(
        "{status_line}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response_body_bytes.len()
      );
      let _ = stream.write_all(response.as_bytes());
      let _ = stream.write_all(response_body_bytes);
      let _ = stream.flush();

      if method.eq_ignore_ascii_case("POST") && path == "/result" {
        break;
      }
    }
  });

  (page_url, rx, join)
}

#[test]
fn ui_worker_drop_files_multiple_submits_all_files_multipart() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let (page_url, rx_request, server_join) = spawn_file_upload_server();

  let dir = tempdir().expect("temp dir");
  let file_a_path = dir.path().join("a.txt");
  let file_b_path = dir.path().join("b.txt");
  let bytes_a = b"upload-a".to_vec();
  let bytes_b = b"upload-b".to_vec();
  std::fs::write(&file_a_path, &bytes_a).expect("write a.txt");
  std::fs::write(&file_b_path, &bytes_b).expect("write b.txt");

  let handle = spawn_ui_worker("fastr-ui-worker-file-input-multiple").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (320, 160), 1.0))
    .expect("viewport");
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      page_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate");

  // Wait for the initial frame so hit testing works.
  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {page_url}"));

  // Drain any queued messages (navigation committed, loading state, etc).
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  ui_tx
    .send(UiToWorker::DropFiles {
      tab_id,
      pos_css: (10.0, 10.0),
      paths: vec![file_a_path.clone(), file_b_path.clone()],
    })
    .expect("drop files");

  // Wait for the updated frame after the drop.
  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("expected FrameReady after dropping files");
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  // Click submit.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 70.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("pointer down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 70.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer up");

  let expected_url = Url::parse(&page_url)
    .expect("parse page url")
    .join("/result")
    .expect("resolve /result")
    .to_string();

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::NavigationStarted { url, .. } if url == &expected_url)
  })
  .expect("expected NavigationStarted");

  // The server should capture the request even if the client side is still rendering /result.
  let captured = rx_request
    .recv_timeout(TIMEOUT)
    .unwrap_or_else(|err| panic!("timed out waiting for server to capture POST /result: {err}"));

  assert_eq!(captured.method, "POST");
  assert_eq!(captured.path, "/result");
  assert!(
    captured.headers.iter().any(|(name, value)| {
      name.eq_ignore_ascii_case("content-type")
        && value.starts_with("multipart/form-data; boundary=fastrender-form-boundary")
    }),
    "expected multipart Content-Type header; got: {:?}",
    captured.headers
  );

  let body_str = String::from_utf8_lossy(&captured.body);
  assert!(
    body_str.contains("filename=\"a.txt\"") && body_str.contains("filename=\"b.txt\""),
    "expected request body to contain both filenames; body={body_str}"
  );
  assert!(
    captured
      .body
      .windows(bytes_a.len())
      .any(|w| w == bytes_a.as_slice()),
    "expected request body to contain a.txt payload"
  );
  assert!(
    captured
      .body
      .windows(bytes_b.len())
      .any(|w| w == bytes_b.as_slice()),
    "expected request body to contain b.txt payload"
  );

  drop(ui_tx);
  join.join().expect("worker join");
  server_join.join().expect("server join");
}

