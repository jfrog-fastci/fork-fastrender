#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{NavigationReason, RepaintReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::time::Duration;

use super::support;

#[test]
fn navigation_creates_a_live_tab_and_ticks_are_safe() {
  let _lock = super::stage_listener_test_lock();
  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-browser-worker-live-tab-test",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");

  let tab_id = TabId(1);

  // Ticking an unknown tab should be a no-op.
  handle
    .ui_tx
    .send(UiToWorker::Tick { tab_id })
    .expect("send Tick(tab)");
  assert!(
    support::drain_for(&handle.ui_rx, Duration::from_millis(50)).is_empty(),
    "expected no worker messages for Tick on unknown tab"
  );

  handle
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("send CreateTab");
  handle
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (32, 32), 1.0))
    .expect("send ViewportChanged");
  handle
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      "about:blank".to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("send Navigate");

  // Wait for the first frame so we know the tab is live.
  let _frame_msg = support::recv_for_tab(&handle.ui_rx, tab_id, support::DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("wait for initial FrameReady");
  // Ensure the navigation completes before ticking.
  let _ = support::recv_for_tab(&handle.ui_rx, tab_id, support::DEFAULT_TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::LoadingState {
        loading: false,
        ..
      }
    )
  })
  .expect("wait for LoadingState(false)");

  // Drain any follow-up messages so only Tick-triggered output remains.
  let _ = support::drain_for(&handle.ui_rx, Duration::from_millis(100));

  // A clean tab should not repaint on tick (this worker currently ignores Tick).
  handle
    .ui_tx
    .send(UiToWorker::Tick { tab_id })
    .expect("send Tick(tab)");
  let drained = support::drain_for(&handle.ui_rx, Duration::from_millis(200));
  assert!(
    drained
      .iter()
      .all(|msg| !matches!(msg, WorkerToUi::FrameReady { .. })),
    "expected no FrameReady after Tick; got:\n{}",
    support::format_messages(&drained)
  );

  // Ensure the worker thread is still alive by requesting an explicit repaint.
  handle
    .ui_tx
    .send(support::request_repaint(tab_id, RepaintReason::Explicit))
    .expect("send RequestRepaint");
  let _ = support::recv_for_tab(&handle.ui_rx, tab_id, support::DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("wait for repaint FrameReady");

  handle.join().expect("join ui worker");
}
