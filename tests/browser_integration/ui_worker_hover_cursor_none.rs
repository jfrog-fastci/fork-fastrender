#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{CursorKind, NavigationReason, PointerButton, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn next_frame_ready(rx: &impl support::RecvTimeout<WorkerToUi>, tab_id: TabId) {
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

fn next_hover_changed(rx: &impl support::RecvTimeout<WorkerToUi>, tab_id: TabId) -> CursorKind {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::HoverChanged { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for HoverChanged for tab {tab_id:?}"));
  match msg {
    WorkerToUi::HoverChanged { cursor, .. } => cursor,
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

#[test]
fn hover_changed_reports_hidden_cursor_for_css_cursor_none() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
            #hidden { position: absolute; top: 0; left: 0; width: 100px; height: 100px; cursor: none; background: rgb(220, 0, 0); }
            #outside { position: absolute; top: 0; left: 120px; width: 120px; height: 100px; background: rgb(0, 220, 0); }
          </style>
        </head>
        <body>
          <div id="hidden"></div>
          <div id="outside"></div>
        </body>
      </html>
    "##,
  );

  let worker = spawn_ui_worker("fastr-ui-worker-hover-cursor-none").expect("spawn ui worker");
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

  // Hover the `cursor: none` region.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (10.0, 10.0),
      PointerButton::None,
    ))
    .unwrap();
  let cursor = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Hidden);

  // Moving out should restore the default cursor kind.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (200.0, 10.0),
      PointerButton::None,
    ))
    .unwrap();
  let cursor = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Default);

  worker.join().unwrap();
}
