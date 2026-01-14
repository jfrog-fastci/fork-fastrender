#![cfg(feature = "browser_ui")]

use super::support::{create_tab_msg, scroll_msg, viewport_changed_msg};
use fastrender::render_control::StageHeartbeat;
use fastrender::ui::about_pages;
use fastrender::ui::messages::{PointerModifiers, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::render_worker::{
  reset_scroll_paint_deadline_timeout_count_for_test, scroll_paint_deadline_timeout_count_for_test,
};
use fastrender::ui::spawn_ui_worker_for_test;
use std::time::{Duration, Instant};

#[test]
fn scroll_paint_deadline_prevents_busy_loop_and_keeps_worker_responsive() {
  let _lock = super::stage_listener_test_lock();

  // Parse env vars once at worker startup. Use an extremely small deadline so scroll-triggered
  // paints are forced to time out, and rely on backoff to prevent the worker from spinning.
  let _env = crate::common::EnvVarsGuard::new(&[
    ("FASTR_SCROLL_PAINT_DEADLINE_MS", Some("0")),
    ("FASTR_SCROLL_PAINT_BACKOFF_MS", Some("20")),
  ]);

  reset_scroll_paint_deadline_timeout_count_for_test();

  // Slow down deadline checks to make deadline timeouts deterministic.
  let handle =
    spawn_ui_worker_for_test("fastr-ui-worker-scroll-paint-deadline", Some(1)).expect("spawn worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(create_tab_msg(
      tab_id,
      Some(about_pages::ABOUT_TEST_SCROLL.to_string()),
    ))
    .expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (220, 120), 1.0))
    .expect("ViewportChanged");

  // Wait for the initial frame so scroll snap/clamping can be computed without waiting for paint.
  super::support::recv_for_tab(&ui_rx, tab_id, Duration::from_secs(20), |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("initial FrameReady");

  // Ensure the channel is quiet before triggering the deadline.
  while ui_rx.try_recv().is_ok() {}

  // Trigger a scroll paint job and wait until it begins so follow-up messages are guaranteed to
  // arrive while a paint is in flight.
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 40.0), None))
    .expect("Scroll");
  super::support::recv_for_tab(&ui_rx, tab_id, Duration::from_secs(10), |msg| {
    matches!(
      msg,
      WorkerToUi::Stage {
        stage: StageHeartbeat::PaintBuild | StageHeartbeat::PaintRasterize,
        ..
      }
    )
  })
  .expect("paint stage heartbeat");

  // While a scroll paint is timing out, the worker should still process new scroll input quickly
  // and emit early scroll-state updates.
  let start = Instant::now();
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 10.0), None))
    .expect("Scroll while paint in flight");
  super::support::recv_for_tab(&ui_rx, tab_id, Duration::from_millis(250), |msg| {
    matches!(msg, WorkerToUi::ScrollStateUpdated { .. })
  })
  .expect("ScrollStateUpdated during scroll paint deadline");
  assert!(
    start.elapsed() < Duration::from_millis(150),
    "ScrollStateUpdated took too long: {:?}",
    start.elapsed()
  );

  // Wait for at least one scroll paint deadline timeout so we know backoff is active.
  let deadline = Instant::now() + Duration::from_secs(2);
  while scroll_paint_deadline_timeout_count_for_test() == 0 && Instant::now() < deadline {
    std::thread::sleep(Duration::from_millis(1));
  }
  assert!(
    scroll_paint_deadline_timeout_count_for_test() > 0,
    "expected at least one scroll paint deadline timeout"
  );

  // Even while a repaint is pending but backed off, the worker should still respond to other
  // messages (no spin-loop that starves the receive path).
  ui_tx
    .send(UiToWorker::ContextMenuRequest {
      tab_id,
      pos_css: (1.0, 1.0),
      modifiers: PointerModifiers::NONE,
    })
    .expect("ContextMenuRequest");
  super::support::recv_for_tab(&ui_rx, tab_id, Duration::from_millis(250), |msg| {
    matches!(msg, WorkerToUi::ContextMenu { .. })
  })
  .expect("ContextMenu response");

  // Sanity check: deadline timeouts should be bounded by the configured backoff. We don't assert an
  // exact count (timing can vary on contended hosts), but it should not explode.
  std::thread::sleep(Duration::from_millis(200));
  let timeouts = scroll_paint_deadline_timeout_count_for_test();
  assert!(
    timeouts < 80,
    "scroll paint deadline timeouts look unbounded: {timeouts}"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

