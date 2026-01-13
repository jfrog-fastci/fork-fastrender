#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  NavigationReason, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tempfile::tempdir;

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

fn spawn_upload_server() -> (
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
    <form action="/upload" method="post" enctype="multipart/form-data">
      <input id="f" type="file" multiple name="up">
      <input id="submit" type="submit" value="Go">
    </form>
  </body>
</html>
"#,
          )
        } else if method.eq_ignore_ascii_case("POST") && path == "/upload" {
          let _ = tx.send(CapturedHttpRequest {
            method: method.clone(),
            path: path.clone(),
            headers: headers.clone(),
            body: body.clone(),
          });
          ("HTTP/1.1 200 OK", "<!doctype html><html><body>ok</body></html>")
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

      if method.eq_ignore_ascii_case("POST") && path == "/upload" {
        break;
      }
    }
  });

  (page_url, rx, join)
}

#[test]
fn file_picker_choose_skips_files_over_env_max_bytes_limit() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  // Use a very small limit so the test does not need to allocate large files.
  let _env = crate::common::global_state::EnvVarGuard::set("FASTR_MAX_FILE_INPUT_BYTES", "8");

  let (page_url, rx_request, server_join) = spawn_upload_server();

  let handle = spawn_ui_worker("fastr-ui-worker-file-input-max-bytes").expect("spawn ui worker");
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

  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  // Click the file input, which should request the file picker.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("pointer down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer up");

  let opened = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FilePickerOpened { .. })
  })
  .unwrap_or_else(|| {
    let msgs = support::drain_for(&ui_rx, Duration::from_millis(200));
    panic!(
      "timed out waiting for FilePickerOpened; saw:\n{}",
      support::format_messages(&msgs)
    )
  });

  let input_node_id = match opened {
    WorkerToUi::FilePickerOpened { input_node_id, .. } => input_node_id,
    _ => unreachable!(),
  };

  // Create a small file and another that exceeds the byte limit.
  let dir = tempdir().expect("temp dir");
  let small_path = dir.path().join("small.txt");
  let large_path = dir.path().join("large.txt");
  let small_bytes = b"small".to_vec(); // 5 bytes
  let large_bytes = b"0123456789abcdef".to_vec(); // 16 bytes
  std::fs::write(&small_path, &small_bytes).expect("write small file");
  std::fs::write(&large_path, &large_bytes).expect("write large file");

  ui_tx
    .send(UiToWorker::FilePickerChoose {
      tab_id,
      input_node_id,
      paths: vec![small_path.clone(), large_path.clone()],
    })
    .expect("file picker choose");

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FilePickerClosed { .. })
  })
  .unwrap_or_else(|| {
    let msgs = support::drain_for(&ui_rx, Duration::from_millis(200));
    panic!(
      "timed out waiting for FilePickerClosed; saw:\n{}",
      support::format_messages(&msgs)
    )
  });

  // Click submit to trigger a multipart form submission.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 70.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("pointer down submit");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 70.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer up submit");

  let captured = rx_request
    .recv_timeout(TIMEOUT)
    .unwrap_or_else(|err| panic!("timed out waiting for server to capture POST /upload: {err}"));
  assert_eq!(captured.method, "POST");
  assert_eq!(captured.path, "/upload");
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
    body_str.contains("filename=\"small.txt\""),
    "expected multipart body to include small.txt; body={body_str}"
  );
  assert!(
    !body_str.contains("filename=\"large.txt\""),
    "expected large.txt to be skipped due to byte limit; body={body_str}"
  );
  assert!(
    captured
      .body
      .windows(small_bytes.len())
      .any(|w| w == small_bytes.as_slice()),
    "expected multipart body to include small file bytes"
  );
  assert!(
    !captured
      .body
      .windows(large_bytes.len())
      .any(|w| w == large_bytes.as_slice()),
    "expected multipart body not to include large file bytes"
  );

  drop(ui_tx);
  join.join().expect("worker join");
  server_join.join().expect("server join");
}

