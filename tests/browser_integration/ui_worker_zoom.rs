#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, RenderedFrame, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

// Keep this generous: these tests do real rendering work and can run under CPU contention.
const TIMEOUT: Duration = Duration::from_secs(20);

fn wait_for_frame_with_meta(
  rx: &fastrender::ui::WorkerToUiInbox,
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
    WorkerToUi::NavigationFailed {
      tab_id: got,
      url,
      error,
      ..
    } => {
      assert_eq!(got, tab_id);
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn assert_close(actual: f32, expected: f32, eps: f32, label: &str) {
  let delta = (actual - expected).abs();
  assert!(
    delta <= eps,
    "{label}: expected ~{expected} got {actual} (delta {delta}, eps {eps})"
  );
}

#[test]
fn zoom_mapping_scales_css_viewport_without_changing_drawn_size() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let handle = spawn_ui_worker("fastr-ui-worker-zoom-mapping").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  // Pick integer values so rounding stays deterministic.
  let available_points = (200.0, 120.0);
  let pixels_per_point = 2.0;

  let tab_id = TabId::new();
  ui_tx.send(support::create_tab_msg(tab_id, None)).unwrap();

  // Zoom=1: normal mapping (viewport_css == available_points, dpr == ppp).
  let (viewport_1, dpr_1) =
    fastrender::ui::viewport_css_and_dpr_for_zoom(available_points, pixels_per_point, 1.0);
  ui_tx
    .send(support::viewport_changed_msg(tab_id, viewport_1, dpr_1))
    .unwrap();
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      "about:newtab".to_string(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  let frame_1 = wait_for_frame_with_meta(&ui_rx, tab_id, viewport_1, dpr_1);
  let pixmap_1 = (frame_1.pixmap.width(), frame_1.pixmap.height());

  // Zoom=2: fewer CSS pixels in the viewport + higher DPR.
  let (viewport_2, dpr_2) =
    fastrender::ui::viewport_css_and_dpr_for_zoom(available_points, pixels_per_point, 2.0);
  assert_ne!(
    viewport_1, viewport_2,
    "viewport_css should change with zoom"
  );
  assert!(dpr_2 > dpr_1, "dpr should increase with zoom");

  ui_tx
    .send(support::viewport_changed_msg(tab_id, viewport_2, dpr_2))
    .unwrap();
  let frame_2 = wait_for_frame_with_meta(&ui_rx, tab_id, viewport_2, dpr_2);
  let pixmap_2 = (frame_2.pixmap.width(), frame_2.pixmap.height());

  // The resulting pixmap size in *device pixels* should stay roughly constant, because we scale
  // viewport_css and dpr inversely.
  assert!(
    (pixmap_1.0 as i64 - pixmap_2.0 as i64).abs() <= 1
      && (pixmap_1.1 as i64 - pixmap_2.1 as i64).abs() <= 1,
    "expected pixmap size to stay ~constant across zoom: {:?} -> {:?}",
    pixmap_1,
    pixmap_2
  );

  // The UI draws the pixmap at `pixmap_px / pixels_per_point` points; ensure that's constant too.
  let drawn_points_1 = (
    pixmap_1.0 as f32 / pixels_per_point,
    pixmap_1.1 as f32 / pixels_per_point,
  );
  let drawn_points_2 = (
    pixmap_2.0 as f32 / pixels_per_point,
    pixmap_2.1 as f32 / pixels_per_point,
  );
  assert_close(
    drawn_points_1.0,
    drawn_points_2.0,
    0.75,
    "drawn width points",
  );
  assert_close(
    drawn_points_1.1,
    drawn_points_2.1,
    0.75,
    "drawn height points",
  );

  // Sanity: the viewport↔DPR pairs should be consistent with the constant device pixel size.
  assert_close(
    viewport_1.0 as f32 * dpr_1,
    viewport_2.0 as f32 * dpr_2,
    1.0,
    "viewport_css*dpr width",
  );
  assert_close(
    viewport_1.1 as f32 * dpr_1,
    viewport_2.1 as f32 * dpr_2,
    1.0,
    "viewport_css*dpr height",
  );

  drop(ui_tx);
  join.join().unwrap();
}
