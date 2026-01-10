#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{CursorKind, NavigationReason, PointerButton, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn next_frame_ready(rx: &Receiver<WorkerToUi>, tab_id: TabId) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));
  match msg {
    WorkerToUi::FrameReady { .. } => {}
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn next_hover_changed(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> (Option<String>, CursorKind) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::HoverChanged { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for HoverChanged for tab {tab_id:?}"));
  match msg {
    WorkerToUi::HoverChanged {
      hovered_url,
      cursor,
      ..
    } => (hovered_url, cursor),
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

#[test]
fn hover_changed_reports_link_url_and_cursor_kind() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page_url = site.write(
    "index.html",
    r##"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body { margin: 0; padding: 0; }
            #link { position: absolute; top: 10px; left: 10px; display: block; width: 120px; height: 24px; background: rgb(220, 220, 0); }
            #input { position: absolute; top: 50px; left: 10px; width: 140px; height: 24px; border: 1px solid #000; }
            #empty { position: absolute; top: 90px; left: 10px; width: 140px; height: 24px; background: rgb(10, 10, 10); }
          </style>
        </head>
        <body>
          <a id="link" href="dest.html#frag">Link</a>
          <input id="input" type="text" value="">
          <div id="empty"></div>
        </body>
      </html>
    "##,
  );

  let expected_hover_url = url::Url::parse(&page_url)
    .expect("parse base url")
    .join("dest.html#frag")
    .expect("resolve href")
    .to_string();

  let worker = spawn_ui_worker("fastr-ui-worker-hover-cursor").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (256, 160), 1.0))
    .unwrap();
  worker
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      page_url,
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  next_frame_ready(&worker.ui_rx, tab_id);

  // Hover link.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 15.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Pointer);
  assert_eq!(hovered_url.as_deref(), Some(expected_hover_url.as_str()));

  // Hover input.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 60.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Text);
  assert_eq!(hovered_url, None);

  // Move to non-interactive region: hovered_url should clear.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 100.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Default);
  assert_eq!(hovered_url, None);

  worker.join().unwrap();
}

