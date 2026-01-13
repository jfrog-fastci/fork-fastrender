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

fn spawn_multipart_upload_server() -> (
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

      let (status_line, response_body) = if method.eq_ignore_ascii_case("GET") && path == "/page.html"
      {
        (
          "HTTP/1.1 200 OK",
          r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      #f { position: absolute; left: 0; top: 0; width: 200px; height: 40px; }
      #s { position: absolute; left: 0; top: 60px; width: 200px; height: 40px; }
    </style>
  </head>
  <body>
    <form action="/upload" method="post" enctype="multipart/form-data">
      <input id="f" type="file" name="f">
      <input id="s" type="submit" value="Upload">
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
fn file_input_submit_multipart_includes_selected_file_bytes() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let (page_url, rx_request, server_join) = spawn_multipart_upload_server();

  let handle =
    spawn_ui_worker("fastr-ui-worker-file-input-submit-multipart").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (240, 160), 1.0))
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

  // Drain any queued messages (navigation committed, loading state, etc) so assertions are scoped
  // to the input interactions.
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

  // Create a temporary file for the upload.
  let tmp = tempfile::tempdir().expect("temp dir");
  let tmp_path = tmp.path().join("upload.txt");
  std::fs::write(&tmp_path, b"hello").expect("write upload file");
  let basename = tmp_path
    .file_name()
    .and_then(|name| name.to_str())
    .unwrap_or("");

  ui_tx
    .send(UiToWorker::FilePickerChoose {
      tab_id,
      input_node_id,
      paths: vec![tmp_path.clone()],
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

  // Click the submit button.
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

  let expected_url = Url::parse(&page_url)
    .expect("parse page url")
    .join("/upload")
    .expect("resolve /upload")
    .to_string();

  support::recv_for_tab(
    &ui_rx,
    tab_id,
    TIMEOUT,
    |msg| matches!(msg, WorkerToUi::NavigationStarted { url, .. } if url == &expected_url),
  )
  .unwrap_or_else(|| {
    let msgs = support::drain_for(&ui_rx, Duration::from_millis(200));
    panic!(
      "timed out waiting for NavigationStarted({expected_url}); saw:\n{}",
      support::format_messages(&msgs)
    )
  });

  support::recv_for_tab(
    &ui_rx,
    tab_id,
    TIMEOUT,
    |msg| matches!(msg, WorkerToUi::NavigationCommitted { url, .. } if url == &expected_url),
  )
  .unwrap_or_else(|| {
    let msgs = support::drain_for(&ui_rx, Duration::from_millis(200));
    panic!(
      "timed out waiting for NavigationCommitted({expected_url}); saw:\n{}",
      support::format_messages(&msgs)
    )
  });

  let captured = rx_request
    .recv_timeout(TIMEOUT)
    .unwrap_or_else(|err| panic!("timed out waiting for server to capture POST /upload: {err}"));
  assert_eq!(captured.method, "POST");
  assert_eq!(captured.path, "/upload");
  assert!(
    captured
      .headers
      .iter()
      .any(|(name, value)| name.eq_ignore_ascii_case("content-type")
        && value == "multipart/form-data; boundary=fastrender-form-boundary"),
    "expected Content-Type multipart/form-data boundary; got: {:?}",
    captured.headers
  );

  let disposition = format!(
    "Content-Disposition: form-data; name=\"f\"; filename=\"{basename}\""
  );
  assert!(
    captured.body.windows(disposition.as_bytes().len()).any(|w| w == disposition.as_bytes()),
    "expected multipart body to contain {disposition:?}; body was:\n{}",
    String::from_utf8_lossy(&captured.body)
  );
  assert!(
    captured.body.windows(b"hello".len()).any(|w| w == b"hello"),
    "expected multipart body to contain file bytes; body was:\n{}",
    String::from_utf8_lossy(&captured.body)
  );

  drop(ui_tx);
  join.join().expect("worker join");
  server_join.join().expect("server join");
}

