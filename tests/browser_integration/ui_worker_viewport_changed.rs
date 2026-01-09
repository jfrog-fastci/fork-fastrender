#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, RenderedFrame, TabId, WorkerToUi};
use fastrender::ui::worker_loop::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);

fn wait_for_navigation_committed(rx: &Receiver<WorkerToUi>, tab_id: TabId, expected_url: &str) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::NavigationFailed { .. })
      || matches!(msg, WorkerToUi::NavigationCommitted { url, .. } if url == expected_url)
  })
  .unwrap_or_else(|| {
    panic!(
      "timed out waiting for NavigationCommitted for tab {tab_id:?} (expected {expected_url})"
    )
  });

  match msg {
    WorkerToUi::NavigationCommitted { tab_id: got, url, .. } => {
      assert_eq!(got, tab_id);
      assert_eq!(url, expected_url);
    }
    WorkerToUi::NavigationFailed { tab_id: got, url, error, .. } => {
      assert_eq!(got, tab_id);
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn wait_for_frame_with_meta(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  expected_viewport_css: (u32, u32),
  expected_dpr: f32,
) -> RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| match msg {
    WorkerToUi::FrameReady { frame, .. } => {
      frame.viewport_css == expected_viewport_css && (frame.dpr - expected_dpr).abs() < 1e-6
    }
    WorkerToUi::NavigationFailed { .. } => true,
    _ => false,
  })
  .unwrap_or_else(|| {
    panic!(
      "timed out waiting for FrameReady for tab {tab_id:?} (viewport_css={expected_viewport_css:?}, dpr={expected_dpr})"
    )
  });

  match msg {
    WorkerToUi::FrameReady { tab_id: got, frame } => {
      assert_eq!(got, tab_id);
      frame
    }
    WorkerToUi::NavigationFailed { tab_id: got, url, error, .. } => {
      assert_eq!(got, tab_id);
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn assert_pixmap_matches_viewport(frame: &RenderedFrame) {
  let expected_w = ((frame.viewport_css.0 as f32) * frame.dpr).round().max(1.0) as i64;
  let expected_h = ((frame.viewport_css.1 as f32) * frame.dpr).round().max(1.0) as i64;
  let actual_w = frame.pixmap.width() as i64;
  let actual_h = frame.pixmap.height() as i64;

  // Allow a small tolerance for rounding differences between the layout/paint pipeline and the
  // test calculation.
  assert!(
    (actual_w - expected_w).abs() <= 1,
    "pixmap width mismatch: expected≈{expected_w}, got {actual_w} (viewport_css={:?}, dpr={})",
    frame.viewport_css,
    frame.dpr
  );
  assert!(
    (actual_h - expected_h).abs() <= 1,
    "pixmap height mismatch: expected≈{expected_h}, got {actual_h} (viewport_css={:?}, dpr={})",
    frame.viewport_css,
    frame.dpr
  );
}

#[test]
fn viewport_changed_after_navigation_emits_new_frame_with_updated_dimensions() {
  let _lock = super::stage_listener_test_lock();
  let site = support::TempSite::new();
  let url = site.write(
    "page.html",
    r#"<!doctype html>
<html>
  <head>
    <style>html, body { margin: 0; padding: 0; background: rgb(10, 20, 30); }</style>
  </head>
  <body></body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-viewport-changed-a").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  // Keep the initial navigation small so the test is fast.
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .unwrap();
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url.clone(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  wait_for_navigation_committed(&ui_rx, tab_id, &url);
  let _initial = wait_for_frame_with_meta(&ui_rx, tab_id, (64, 64), 1.0);

  ui_tx
    .send(support::viewport_changed_msg(tab_id, (120, 80), 1.0))
    .unwrap();

  let frame = wait_for_frame_with_meta(&ui_rx, tab_id, (120, 80), 1.0);
  assert_eq!(frame.viewport_css, (120, 80));
  assert!((frame.dpr - 1.0).abs() < 1e-6);
  assert_pixmap_matches_viewport(&frame);

  drop(ui_tx);
  join.join().unwrap();
}

#[test]
fn viewport_changed_updates_dpr_and_pixmap_scale() {
  let _lock = super::stage_listener_test_lock();
  let site = support::TempSite::new();
  let url = site.write(
    "page.html",
    r#"<!doctype html>
<html>
  <head>
    <style>html, body { margin: 0; padding: 0; background: rgb(80, 90, 100); }</style>
  </head>
  <body></body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-viewport-changed-b").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (90, 60), 1.0))
    .unwrap();
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url.clone(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  wait_for_navigation_committed(&ui_rx, tab_id, &url);
  let frame_1x = wait_for_frame_with_meta(&ui_rx, tab_id, (90, 60), 1.0);
  let w1 = frame_1x.pixmap.width() as i64;
  let h1 = frame_1x.pixmap.height() as i64;
  assert_pixmap_matches_viewport(&frame_1x);

  ui_tx
    .send(support::viewport_changed_msg(tab_id, (90, 60), 2.0))
    .unwrap();

  let frame_2x = wait_for_frame_with_meta(&ui_rx, tab_id, (90, 60), 2.0);
  assert!((frame_2x.dpr - 2.0).abs() < 1e-6);
  assert_pixmap_matches_viewport(&frame_2x);

  let w2 = frame_2x.pixmap.width() as i64;
  let h2 = frame_2x.pixmap.height() as i64;
  assert!(
    (w2 - w1 * 2).abs() <= 1,
    "expected pixmap width to scale ~2x when dpr doubles: {w1} -> {w2}"
  );
  assert!(
    (h2 - h1 * 2).abs() <= 1,
    "expected pixmap height to scale ~2x when dpr doubles: {h1} -> {h2}"
  );

  drop(ui_tx);
  join.join().unwrap();
}
