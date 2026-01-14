#![cfg(feature = "browser_ui")]

use crate::common::net::{net_test_lock, try_bind_localhost};
use fastrender::resource::{FetchedResource, ResourceFetcher};
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::render_worker::{
  renderer_build_count_for_test, reset_renderer_build_count_for_test, spawn_ui_worker_with_factory,
};
use fastrender::ui::spawn_ui_worker;
use std::io;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use super::support;

// These tests perform real navigations + paints; keep timeout generous for contended CI hosts.
const TIMEOUT: Duration = Duration::from_secs(20);
const SERVER_WAIT: Duration = Duration::from_secs(10);

fn next_navigation_committed(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
) -> String {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationCommitted { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationCommitted for tab {tab_id:?}"));

  match msg {
    WorkerToUi::NavigationCommitted { url, .. } => url,
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}")
    }
    other => {
      panic!("unexpected WorkerToUi message while waiting for NavigationCommitted: {other:?}")
    }
  }
}

#[derive(Debug, Default)]
struct MismatchedFinalUrlFetcher;

impl ResourceFetcher for MismatchedFinalUrlFetcher {
  fn fetch(&self, url: &str) -> fastrender::Result<FetchedResource> {
    let html = b"<!doctype html><meta charset=\"utf-8\"><title>ok</title><body>ok</body>";
    match url {
      // Simulate a renderer/fetch layer reporting a different final URL (as if redirected) than
      // the navigation was started with.
      "http://a.test/start" => Ok(FetchedResource::with_final_url(
        html.to_vec(),
        Some("text/html".to_string()),
        Some("http://b.test/target".to_string()),
      )),
      // The restart navigation should run for the final site and commit normally.
      "http://b.test/target" => Ok(FetchedResource::with_final_url(
        html.to_vec(),
        Some("text/html".to_string()),
        Some("http://b.test/target".to_string()),
      )),
      other => Err(fastrender::Error::Other(format!(
        "MismatchedFinalUrlFetcher received unexpected URL: {other}"
      ))),
    }
  }
}

#[test]
fn ui_worker_restarts_navigation_on_committed_url_site_mismatch() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  reset_renderer_build_count_for_test();

  let fetcher = Arc::new(MismatchedFinalUrlFetcher::default());
  let factory = support::deterministic_factory_with_fetcher(fetcher).expect("factory");
  let handle =
    spawn_ui_worker_with_factory("fastr-ui-worker-site-mismatch-fetcher", factory).unwrap();
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, Some("about:newtab".to_string())))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .expect("viewport");

  assert_eq!(next_navigation_committed(&ui_rx, tab_id), "about:newtab");

  ui_tx
    .send(support::navigate_msg(
      tab_id,
      "http://a.test/start".to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate start");

  assert_eq!(
    next_navigation_committed(&ui_rx, tab_id),
    "http://b.test/target"
  );

  // We should have built:
  // - one renderer for the initial about:newtab navigation
  // - one renderer for the (wrong-site) navigation attempt
  // - one renderer for the restarted navigation in the committed site process
  assert_eq!(
    renderer_build_count_for_test(),
    3,
    "expected a renderer rebuild for the committed-site process swap",
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

fn read_http_request(stream: &mut std::net::TcpStream) {
  let mut buf = Vec::new();
  let mut tmp = [0u8; 1024];
  loop {
    match stream.read(&mut tmp) {
      Ok(0) => break,
      Ok(n) => {
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
          break;
        }
        if buf.len() > 64 * 1024 {
          break;
        }
      }
      Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
        thread::sleep(Duration::from_millis(5));
      }
      Err(_) => break,
    }
  }
}

fn spawn_redirect_server(listener: TcpListener, location: String) -> thread::JoinHandle<()> {
  thread::spawn(move || {
    let _ = listener.set_nonblocking(true);
    let start = Instant::now();
    while start.elapsed() < SERVER_WAIT {
      match listener.accept() {
        Ok((mut stream, _)) => {
          let _ = stream.set_nonblocking(true);
          read_http_request(&mut stream);
          let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
          );
          let _ = stream.write_all(response.as_bytes());
          return;
        }
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
          thread::sleep(Duration::from_millis(5));
        }
        Err(_) => break,
      }
    }
  })
}

fn spawn_html_server(
  listener: TcpListener,
  body: String,
  expected_connections: usize,
) -> thread::JoinHandle<()> {
  thread::spawn(move || {
    let _ = listener.set_nonblocking(true);
    let start = Instant::now();
    let mut served = 0usize;
    while start.elapsed() < SERVER_WAIT {
      match listener.accept() {
        Ok((mut stream, _)) => {
          let _ = stream.set_nonblocking(true);
          read_http_request(&mut stream);
          let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.as_bytes().len()
          );
          let _ = stream.write_all(response.as_bytes());
          let _ = stream.write_all(body.as_bytes());
          served += 1;
          if served >= expected_connections {
            return;
          }
        }
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
          thread::sleep(Duration::from_millis(5));
        }
        Err(_) => break,
      }
    }
  })
}

#[test]
fn ui_worker_process_swaps_on_cross_site_redirect_commit() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let _net_guard = net_test_lock();
  reset_renderer_build_count_for_test();

  let Some(listener_b) =
    try_bind_localhost("ui_worker_process_swaps_on_cross_site_redirect_commit (target)") else {
    return;
  };
  let Some(listener_a) =
    try_bind_localhost("ui_worker_process_swaps_on_cross_site_redirect_commit (redirect)") else {
    return;
  };

  let addr_b = listener_b.local_addr().expect("addr_b");
  let addr_a = listener_a.local_addr().expect("addr_a");
  let url_b = format!("http://{addr_b}/target");
  let url_a = format!("http://{addr_a}/start");

  // With the committed-url mismatch check enabled, the browser will:
  // - request A, follow redirect to B (first B request)
  // - detect that the commit site is B but the process site is A
  // - restart navigation directly to B (second B request)
  let html_body =
    "<!doctype html><meta charset=\"utf-8\"><title>ok</title><body>ok</body>".to_string();
  let server_b = spawn_html_server(listener_b, html_body, 2);
  let server_a = spawn_redirect_server(listener_a, url_b.clone());

  let handle = spawn_ui_worker("fastr-ui-worker-site-mismatch-redirect").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, Some("about:newtab".to_string())))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .expect("viewport");

  assert_eq!(next_navigation_committed(&ui_rx, tab_id), "about:newtab");

  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url_a.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate redirect");
  assert_eq!(next_navigation_committed(&ui_rx, tab_id), url_b);

  assert_eq!(
    renderer_build_count_for_test(),
    3,
    "expected a renderer rebuild for the committed-site process swap after redirect",
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
  server_a.join().expect("join server_a");
  server_b.join().expect("join server_b");
}
