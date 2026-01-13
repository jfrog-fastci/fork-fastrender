#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, PageDragKind, PointerButton, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn next_frame_ready(rx: &Receiver<WorkerToUi>, tab_id: TabId) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  if let WorkerToUi::NavigationFailed { url, error, .. } = msg {
    panic!("navigation failed for {url}: {error}");
  }
}

fn next_page_drag_started(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> (PageDragKind, String) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::PageDragStarted { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for PageDragStarted for tab {tab_id:?}"));

  match msg {
    WorkerToUi::PageDragStarted { kind, payload, .. } => (kind, payload),
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

#[test]
fn link_drag_emits_page_drag_payload() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page_url = site.write(
    "index.html",
    r##"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body { margin: 0; padding: 0; }
            #link { position: absolute; left: 10px; top: 10px; display: block; width: 120px; height: 24px; background: rgb(220, 220, 0); }
          </style>
        </head>
        <body>
          <a id="link" href="dest.html">Link</a>
        </body>
      </html>
    "##,
  );
  let expected_payload = url::Url::parse(&page_url)
    .expect("parse base url")
    .join("dest.html")
    .expect("resolve href")
    .to_string();

  let worker = spawn_ui_worker("fastr-ui-worker-link-drag-payload").expect("spawn ui worker");
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

  // Begin a primary press on the link and move past the drag threshold.
  worker
    .ui_tx
    .send(support::pointer_down(
      tab_id,
      (15.0, 15.0),
      PointerButton::Primary,
    ))
    .unwrap();
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (40.0, 40.0),
      PointerButton::Primary,
    ))
    .unwrap();

  let (kind, payload) = next_page_drag_started(&worker.ui_rx, tab_id);
  assert_eq!(kind, PageDragKind::Link);
  assert_eq!(payload, expected_payload);
}

