#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::render_worker::{
  renderer_build_count_for_test, reset_renderer_build_count_for_test,
};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

// This test performs real navigations + paints; keep timeout generous for contended CI hosts.
const TIMEOUT: Duration = Duration::from_secs(20);

fn next_navigation_committed(rx: &fastrender::ui::WorkerToUiInbox, tab_id: TabId) -> String {
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

fn next_frame_ready(
  rx: &fastrender::ui::WorkerToUiInbox,
  tab_id: TabId,
) -> fastrender::ui::messages::RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("unexpected WorkerToUi message while waiting for FrameReady: {other:?}"),
  }
}

fn simple_color_page(color: &str) -> String {
  format!(
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body {{ margin: 0; padding: 0; background: {color}; }}
    </style>
  </head>
  <body></body>
</html>"#
  )
}

#[test]
fn ui_worker_reuses_single_renderer_per_tab_across_navigations() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  reset_renderer_build_count_for_test();

  let site = support::TempSite::new();
  let url_a = site.write("a.html", &simple_color_page("rgb(255, 0, 0)"));
  let url_b = site.write("b.html", &simple_color_page("rgb(0, 255, 0)"));

  let handle = spawn_ui_worker("fastr-ui-worker-renderer-reuse").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .expect("viewport");

  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url_a.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate a");
  assert_eq!(next_navigation_committed(&ui_rx, tab_id), url_a);
  let frame_a = next_frame_ready(&ui_rx, tab_id);
  assert_eq!(support::rgba_at(&frame_a.pixmap, 1, 1), [255, 0, 0, 255]);

  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url_b.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate b");
  assert_eq!(next_navigation_committed(&ui_rx, tab_id), url_b);
  let frame_b = next_frame_ready(&ui_rx, tab_id);
  assert_eq!(support::rgba_at(&frame_b.pixmap, 1, 1), [0, 255, 0, 255]);

  // Navigate again in the same tab; the worker should reuse the original tab-level renderer.
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url_a.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate a again");
  assert_eq!(next_navigation_committed(&ui_rx, tab_id), url_a);
  let _ = next_frame_ready(&ui_rx, tab_id);

  assert_eq!(
    renderer_build_count_for_test(),
    1,
    "expected UI worker to build exactly one renderer for this tab across navigations",
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
