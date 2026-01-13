#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  NavigationReason, PointerButton, RenderedFrame, RepaintReason, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

// These tests can be CPU-heavy (layout + paint) and run in parallel with other integration tests
// (CI), so keep a generous timeout to avoid flakiness.
const TIMEOUT: Duration = Duration::from_secs(20);

fn fixture() -> (support::TempSite, String) {
  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #box { width:64px; height:64px; background: rgb(255,0,0); }
      #box:hover { background: rgb(0,255,0); }
      #box:active { background: rgb(0,0,255); }
    </style>
  </head>
  <body>
    <div id="box"></div>
  </body>
</html>
"#,
  );
  (site, url)
}

fn scroll_hover_fixture() -> (support::TempSite, String) {
  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #a, #b { width: 100%; height: 50px; }
      #a { background: rgb(255,0,0); }
      #a:hover { background: rgb(0,0,255); }
      #b { background: rgb(0,255,0); }
      #b:hover { background: rgb(255,255,0); }
      .spacer { height: 400px; }
    </style>
  </head>
  <body>
    <div id="a"></div>
    <div id="b"></div>
    <div class="spacer"></div>
  </body>
</html>
"#,
  );
  (site, url)
}

fn rgba_at_css(frame: &RenderedFrame, x_css: u32, y_css: u32) -> [u8; 4] {
  let x_px = ((x_css as f32) * frame.dpr).round() as u32;
  let y_px = ((y_css as f32) * frame.dpr).round() as u32;
  support::rgba_at(&frame.pixmap, x_px, y_px)
}

fn expect_rgb_at_css(frame: &RenderedFrame, x_css: u32, y_css: u32, expected: (u8, u8, u8)) {
  let rgba = rgba_at_css(frame, x_css, y_css);
  assert_eq!(
    (rgba[0], rgba[1], rgba[2], rgba[3]),
    (expected.0, expected.1, expected.2, 255),
    "unexpected pixel at ({x_css},{y_css}) css px"
  );
}

fn next_frame_ready(rx: &fastrender::ui::WorkerToUiInbox, tab_id: TabId) -> RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  match msg {
    WorkerToUi::FrameReady {
      tab_id: msg_tab,
      frame,
    } => {
      assert_eq!(msg_tab, tab_id);
      frame
    }
    WorkerToUi::NavigationFailed {
      tab_id: msg_tab,
      url,
      error,
      ..
    } => {
      assert_eq!(msg_tab, tab_id);
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

#[test]
fn pointer_move_sets_hover_and_repaints() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_site, url) = fixture();

  let worker = spawn_ui_worker("fastr-ui-worker-hover-active-a").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (256, 256), 1.0))
    .unwrap();
  worker
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      url,
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  expect_rgb_at_css(&frame, 10, 10, (255, 0, 0));

  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (10.0, 10.0),
      PointerButton::None,
    ))
    .unwrap();
  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  expect_rgb_at_css(&frame, 10, 10, (0, 255, 0));

  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (200.0, 200.0),
      PointerButton::None,
    ))
    .unwrap();
  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  expect_rgb_at_css(&frame, 10, 10, (255, 0, 0));

  worker.join().unwrap();
}

#[test]
fn pointer_down_sets_active_until_pointer_up() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_site, url) = fixture();

  let worker = spawn_ui_worker("fastr-ui-worker-hover-active-b").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (256, 256), 1.0))
    .unwrap();
  worker
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      url,
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  expect_rgb_at_css(&frame, 10, 10, (255, 0, 0));

  // Make hover deterministic so PointerUp resolves to green rather than red.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (10.0, 10.0),
      PointerButton::None,
    ))
    .unwrap();
  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  expect_rgb_at_css(&frame, 10, 10, (0, 255, 0));

  worker
    .ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .unwrap();
  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  expect_rgb_at_css(&frame, 10, 10, (0, 0, 255));

  worker
    .ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .unwrap();
  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  expect_rgb_at_css(&frame, 10, 10, (0, 255, 0));

  worker.join().unwrap();
}

#[test]
fn scroll_with_pointer_updates_hover_target() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_site, url) = scroll_hover_fixture();

  let worker = spawn_ui_worker("fastr-ui-worker-hover-scroll").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (256, 128), 1.0))
    .unwrap();
  worker
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      url,
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  expect_rgb_at_css(&frame, 10, 10, (255, 0, 0));

  // Hover the first box.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (10.0, 10.0),
      PointerButton::None,
    ))
    .unwrap();
  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  expect_rgb_at_css(&frame, 10, 10, (0, 0, 255));

  // Scroll the viewport down so the second box moves under the cursor position.
  worker
    .ui_tx
    .send(support::scroll_msg(tab_id, (0.0, 60.0), Some((10.0, 10.0))))
    .unwrap();
  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  // Box #b should now be under the cursor and hovered.
  expect_rgb_at_css(&frame, 10, 10, (255, 255, 0));

  worker.join().unwrap();
}

#[test]
fn activating_tab_clears_hover_state() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_site, url) = fixture();

  let worker = spawn_ui_worker("fastr-ui-worker-hover-active-tab").expect("spawn ui worker");

  let tab_a = TabId(1);
  let tab_b = TabId(2);

  worker
    .ui_tx
    .send(support::create_tab_msg(tab_a, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_b, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_a, (256, 256), 1.0))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_b, (256, 256), 1.0))
    .unwrap();

  worker
    .ui_tx
    .send(support::navigate_msg(
      tab_a,
      url,
      NavigationReason::TypedUrl,
    ))
    .unwrap();
  let frame = next_frame_ready(&worker.ui_rx, tab_a);
  expect_rgb_at_css(&frame, 10, 10, (255, 0, 0));

  worker
    .ui_tx
    .send(support::pointer_move(
      tab_a,
      (10.0, 10.0),
      PointerButton::None,
    ))
    .unwrap();
  let frame = next_frame_ready(&worker.ui_rx, tab_a);
  expect_rgb_at_css(&frame, 10, 10, (0, 255, 0));

  worker
    .ui_tx
    .send(UiToWorker::SetActiveTab { tab_id: tab_b })
    .unwrap();
  worker
    .ui_tx
    .send(UiToWorker::SetActiveTab { tab_id: tab_a })
    .unwrap();
  worker
    .ui_tx
    .send(support::request_repaint(tab_a, RepaintReason::Explicit))
    .unwrap();

  let frame = next_frame_ready(&worker.ui_rx, tab_a);
  expect_rgb_at_css(&frame, 10, 10, (255, 0, 0));

  worker.join().unwrap();
}
