#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;

use super::support::{create_tab_msg, scroll_to_msg, viewport_changed_msg, DEFAULT_TIMEOUT};

#[test]
fn ui_worker_reports_scroll_metrics_and_scroll_to_updates_scroll_state() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let handle = spawn_ui_worker("fastr-ui-worker-scroll-metrics").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(create_tab_msg(
      tab_id,
      Some(fastrender::ui::about_pages::ABOUT_TEST_SCROLL.to_string()),
    ))
    .expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("ViewportChanged");

  let msg = super::support::recv_for_tab(&ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("FrameReady");
  let WorkerToUi::FrameReady { frame, .. } = msg else {
    unreachable!();
  };

  // About page template:
  //   .spacer { height: 4000px; }
  //
  // Scroll max should be content-height minus viewport height.
  let expected_content_h = 4000.0;
  let expected_max_scroll_y = expected_content_h - 100.0;

  assert_eq!(
    frame.scroll_metrics.viewport_css,
    (200, 100),
    "scroll metrics should report the viewport size used for this frame"
  );

  assert!(
    (frame.scroll_metrics.content_css.1 - expected_content_h).abs() < 2.0,
    "unexpected content height in scroll metrics: got {}, expected ~{}",
    frame.scroll_metrics.content_css.1,
    expected_content_h
  );

  assert!(
    (frame.scroll_metrics.bounds_css.max_y - expected_max_scroll_y).abs() < 2.0,
    "unexpected max scroll y in scroll metrics: got {}, expected ~{}",
    frame.scroll_metrics.bounds_css.max_y,
    expected_max_scroll_y
  );

  // Drain the initial ScrollStateUpdated message sent after the first frame so our subsequent wait
  // sees the scroll-to update.
  let _ = super::support::recv_for_tab(&ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ScrollStateUpdated { .. })
  });

  let target_y = (frame.scroll_metrics.bounds_css.max_y * 0.5).round();
  ui_tx
    .send(scroll_to_msg(tab_id, (0.0, target_y)))
    .expect("ScrollTo");

  let msg = super::support::recv_for_tab(&ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| match msg {
    WorkerToUi::ScrollStateUpdated { scroll, .. } => (scroll.viewport.y - target_y).abs() < 2.0,
    _ => false,
  })
  .expect("ScrollStateUpdated after ScrollTo");
  let WorkerToUi::ScrollStateUpdated { scroll, .. } = msg else {
    unreachable!();
  };

  assert!(
    (scroll.viewport.y - target_y).abs() < 2.0,
    "expected ScrollTo to update viewport scroll to ~{target_y}, got {:?}",
    scroll.viewport
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
