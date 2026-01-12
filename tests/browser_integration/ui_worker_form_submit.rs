#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  KeyAction, NavigationReason, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi,
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

#[test]
fn click_submit_navigates_to_get_form_submission_url() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page_url = site.write(
    "page.html",
    r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #q { position: absolute; left: 0; top: 60px; width: 120px; height: 24px; }
      #submit { position: absolute; left: 0; top: 0; width: 120px; height: 40px; }
    </style>
  </head>
  <body>
    <form action="result.html">
      <input id="q" name="q" value="a b">
      <input id="submit" type="submit" value="Go">
    </form>
  </body>
</html>
"#,
  );
  let _result_url = site.write(
    "result.html",
    r#"<!doctype html>
<html><body>ok</body></html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-form-submit").expect("spawn ui worker");
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
  // to the submit click.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

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

  let mut expected = Url::parse(&page_url)
    .expect("parse page url")
    .join("result.html")
    .expect("resolve result.html");
  expected.set_query(Some("q=a+b"));
  let expected_url = expected.to_string();

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
    );
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
    );
  });

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn enter_in_text_input_navigates_to_get_form_submission_url() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page_url = site.write(
    "page.html",
    r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #q { position: absolute; left: 0; top: 60px; width: 120px; height: 24px; }
      #submit { position: absolute; left: 0; top: 0; width: 120px; height: 40px; }
    </style>
  </head>
  <body>
    <form action="result.html">
      <input id="q" name="q" value="a b">
      <input id="submit" type="submit" value="Go">
    </form>
  </body>
</html>
"#,
  );
  let _result_url = site.write(
    "result.html",
    r#"<!doctype html>
<html><body>ok</body></html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-form-submit-enter").expect("spawn ui worker");
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
  // to the submit triggered by Enter.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  // Click the input to focus it, then press Enter.
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
  ui_tx
    .send(UiToWorker::KeyAction {
      tab_id,
      key: KeyAction::Enter,
    })
    .expect("key enter");

  let mut expected = Url::parse(&page_url)
    .expect("parse page url")
    .join("result.html")
    .expect("resolve result.html");
  expected.set_query(Some("q=a+b"));
  let expected_url = expected.to_string();

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
    );
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
    );
  });

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn enter_in_text_input_without_submitter_navigates_even_if_input_click_prevent_default() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page_url = site.write(
    "page.html",
    r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #q { position: absolute; left: 0; top: 0; width: 120px; height: 24px; }
    </style>
  </head>
  <body>
    <form action="result.html">
      <input id="q" name="q" value="a b">
    </form>
    <script>
      // Enter key submission should not synthesize a click event on the focused input, so a click
      // preventDefault handler must not block the implicit form submission.
      document.getElementById("q").addEventListener("click", function (ev) { ev.preventDefault(); });
    </script>
  </body>
</html>
"#,
  );
  let _result_url = site.write(
    "result.html",
    r#"<!doctype html>
<html><body>ok</body></html>
"#,
  );

  let handle =
    spawn_ui_worker("fastr-ui-worker-form-submit-enter-no-submitter").expect("spawn ui worker");
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

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {page_url}"));

  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

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
  ui_tx
    .send(UiToWorker::KeyAction {
      tab_id,
      key: KeyAction::Enter,
    })
    .expect("key enter");

  let mut expected = Url::parse(&page_url)
    .expect("parse page url")
    .join("result.html")
    .expect("resolve result.html");
  expected.set_query(Some("q=a+b"));
  let expected_url = expected.to_string();

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
    );
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
    );
  });

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn submit_prevent_default_blocks_enter_form_submission_navigation_without_submitter() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let next_url = site.write(
    "result.html",
    r#"<!doctype html>
<html><body>next</body></html>
"#,
  );
  let page_url = site.write(
    "page.html",
    r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #q { position: absolute; left: 0; top: 0; width: 120px; height: 24px; }
    </style>
  </head>
  <body>
    <form id="f" action="result.html">
      <input id="q" name="q" value="a b">
    </form>
    <script>
      document.getElementById("f").addEventListener("submit", function (ev) { ev.preventDefault(); });
    </script>
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-form-submit-prevent-default-no-submitter")
    .expect("spawn ui worker");
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

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {page_url}"));

  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

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
  ui_tx
    .send(UiToWorker::KeyAction {
      tab_id,
      key: KeyAction::Enter,
    })
    .expect("key enter");

  let msgs = support::drain_for(&ui_rx, Duration::from_millis(500));
  assert!(
    !msgs.iter().any(|msg| {
      matches!(
        msg,
        WorkerToUi::NavigationStarted { .. }
          | WorkerToUi::NavigationCommitted { .. }
          | WorkerToUi::NavigationFailed { .. }
          | WorkerToUi::RequestOpenInNewTab { .. }
      )
    }),
    "expected Enter key submission to honor submit preventDefault and suppress navigation to {next_url}; got:\n{}",
    support::format_messages(&msgs)
  );

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn submit_prevent_default_blocks_click_submit_navigation() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let next_url = site.write(
    "result.html",
    r#"<!doctype html>
<html><body>next</body></html>
"#,
  );
  let page_url = site.write(
    "page.html",
    r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #submit { position: absolute; left: 0; top: 0; width: 120px; height: 40px; }
      #q { position: absolute; left: 0; top: 60px; width: 120px; height: 24px; }
    </style>
  </head>
  <body>
    <form id="f" action="result.html">
      <input id="q" name="q" value="a b">
      <input id="submit" type="submit" value="Go">
    </form>
    <script>
      document.getElementById("f").addEventListener("submit", function (ev) { ev.preventDefault(); });
    </script>
  </body>
</html>
"#,
  );

  let handle =
    spawn_ui_worker("fastr-ui-worker-form-submit-prevent-default").expect("spawn ui worker");
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

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {page_url}"));
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // Click submit. The JS `submit` listener prevents default, so no navigation should occur.
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

  let msgs = support::drain_for(&ui_rx, Duration::from_millis(500));
  assert!(
    !msgs.iter().any(|msg| {
      matches!(
        msg,
        WorkerToUi::NavigationStarted { .. }
          | WorkerToUi::NavigationCommitted { .. }
          | WorkerToUi::NavigationFailed { .. }
          | WorkerToUi::RequestOpenInNewTab { .. }
      )
    }),
    "expected submit preventDefault to suppress navigation to {next_url}; got:\n{}",
    support::format_messages(&msgs)
  );

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn submit_prevent_default_blocks_enter_form_submission_navigation() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let next_url = site.write(
    "result.html",
    r#"<!doctype html>
<html><body>next</body></html>
"#,
  );
  let page_url = site.write(
    "page.html",
    r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #submit { position: absolute; left: 0; top: 0; width: 120px; height: 40px; }
      #q { position: absolute; left: 0; top: 60px; width: 120px; height: 24px; }
    </style>
  </head>
  <body>
    <form id="f" action="result.html">
      <input id="q" name="q" value="a b">
      <input id="submit" type="submit" value="Go">
    </form>
    <script>
      document.getElementById("f").addEventListener("submit", function (ev) { ev.preventDefault(); });
    </script>
  </body>
</html>
"#,
  );

  let handle =
    spawn_ui_worker("fastr-ui-worker-form-submit-enter-prevent-default").expect("spawn ui worker");
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

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {page_url}"));
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // Focus the input then press Enter to submit.
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
  ui_tx
    .send(UiToWorker::KeyAction {
      tab_id,
      key: KeyAction::Enter,
    })
    .expect("key enter");

  let msgs = support::drain_for(&ui_rx, Duration::from_millis(500));
  assert!(
    !msgs.iter().any(|msg| {
      matches!(
        msg,
        WorkerToUi::NavigationStarted { .. }
          | WorkerToUi::NavigationCommitted { .. }
          | WorkerToUi::NavigationFailed { .. }
          | WorkerToUi::RequestOpenInNewTab { .. }
      )
    }),
    "expected submit preventDefault to suppress navigation to {next_url}; got:\n{}",
    support::format_messages(&msgs)
  );

  drop(ui_tx);
  join.join().expect("worker join");
}

#[derive(Debug)]
struct CapturedHttpRequest {
  method: String,
  path: String,
  headers: Vec<(String, String)>,
  body: Vec<u8>,
}

fn spawn_form_post_server() -> (
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
      #q { position: absolute; left: 0; top: 60px; width: 120px; height: 24px; }
      #submit { position: absolute; left: 0; top: 0; width: 120px; height: 40px; }
    </style>
  </head>
  <body>
    <form action="/result" method="post">
      <input id="q" name="q" value="a b">
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
fn click_submit_navigates_with_post_urlencoded_form_submission() {
  let _lock = super::stage_listener_test_lock();

  let (page_url, rx_request, server_join) = spawn_form_post_server();

  let handle = spawn_ui_worker("fastr-ui-worker-form-submit-post").expect("spawn ui worker");
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
  // to the submit click.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

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

  let expected_url = Url::parse(&page_url)
    .expect("parse page url")
    .join("/result")
    .expect("resolve /result")
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
    );
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
    );
  });

  let captured = rx_request
    .recv_timeout(TIMEOUT)
    .unwrap_or_else(|err| panic!("timed out waiting for server to capture POST /result: {err}"));
  assert_eq!(captured.method, "POST");
  assert_eq!(captured.path, "/result");
  assert_eq!(captured.body, b"q=a+b".to_vec());
  assert!(
    captured
      .headers
      .iter()
      .any(|(name, value)| name.eq_ignore_ascii_case("content-type")
        && value.starts_with("application/x-www-form-urlencoded")),
    "expected Content-Type header; got: {:?}",
    captured.headers
  );

  drop(ui_tx);
  join.join().expect("worker join");
  server_join.join().expect("server join");
}
