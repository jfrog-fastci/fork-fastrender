#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::interaction::hit_test::{
  hit_test_dom_call_count_for_test, reset_hit_test_dom_call_count_for_test,
  set_hit_test_dom_counting_enabled_for_test,
};
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

fn next_hover_changed(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
) -> (Option<String>, CursorKind) {
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
fn ui_worker_pointer_move_hit_tests_only_once() {
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
            #link { position: absolute; top: 10px; left: 10px; display: block; width: 120px; height: 24px; background: rgb(220, 220, 0); }
          </style>
        </head>
        <body>
          <a id="link" href="dest.html">Link</a>
        </body>
      </html>
    "##,
  );

  let worker = spawn_ui_worker("fastr-ui-worker-pointer-move-hit-test-dedup").expect("spawn worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (256, 64), 1.0))
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

  set_hit_test_dom_counting_enabled_for_test(true);
  reset_hit_test_dom_call_count_for_test();

  // A single pointer move should perform a single DOM hit test, reused by the interaction engine and
  // hover UI bookkeeping.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 15.0),
      PointerButton::None,
    ))
    .unwrap();
  let (_hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Pointer);

  assert_eq!(
    hit_test_dom_call_count_for_test(),
    1,
    "pointer move should hit-test once (not twice)"
  );

  set_hit_test_dom_counting_enabled_for_test(false);
}
