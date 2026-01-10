#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
use std::time::Duration;

const ACTIVE_FRAME_TIMEOUT: Duration = Duration::from_secs(1);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

#[test]
fn active_tab_scroll_is_not_blocked_by_background_heavy_navigation() {
  let _lock = super::stage_listener_test_lock();

  let worker = fastrender::ui::spawn_browser_worker_for_test(Some(1)).expect("spawn worker");

  let tab_active = TabId::new();
  let tab_bg = TabId::new();
  let cancel_active = CancelGens::new();
  let cancel_bg = CancelGens::new();

  worker
    .tx
    .send(support::create_tab_msg_with_cancel(
      tab_active,
      Some("about:blank".to_string()),
      cancel_active.clone(),
    ))
    .expect("CreateTab active");
  worker
    .tx
    .send(support::create_tab_msg_with_cancel(
      tab_bg,
      Some("about:blank".to_string()),
      cancel_bg.clone(),
    ))
    .expect("CreateTab background");

  worker
    .tx
    .send(UiToWorker::SetActiveTab { tab_id: tab_active })
    .expect("SetActiveTab");

  cancel_active.bump_paint();
  worker
    .tx
    .send(support::viewport_changed_msg(tab_active, (240, 160), 1.0))
    .expect("ViewportChanged active");
  cancel_bg.bump_paint();
  worker
    .tx
    .send(support::viewport_changed_msg(tab_bg, (240, 160), 1.0))
    .expect("ViewportChanged background");

  support::recv_for_tab(&worker.rx, tab_active, STARTUP_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("initial FrameReady active");
  support::recv_for_tab(&worker.rx, tab_bg, STARTUP_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("initial FrameReady background");

  let _ = support::drain_for(&worker.rx, Duration::from_millis(50));

  cancel_bg.bump_nav();
  worker
    .tx
    .send(support::navigate_msg(
      tab_bg,
      "about:test-heavy".to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("Navigate heavy background");

  support::recv_for_tab(&worker.rx, tab_bg, STARTUP_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::NavigationStarted { .. })
  })
  .expect("NavigationStarted background");
  support::recv_for_tab(&worker.rx, tab_bg, STARTUP_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::Stage { .. })
  })
  .expect("Stage heartbeat background");

  cancel_active.bump_paint();
  worker
    .tx
    .send(support::scroll_msg(tab_active, (0.0, 200.0), None))
    .expect("Scroll active");

  support::recv_for_tab(&worker.rx, tab_active, ACTIVE_FRAME_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("FrameReady for active tab after scroll");

  cancel_bg.bump_nav();
  worker
    .tx
    .send(UiToWorker::CloseTab { tab_id: tab_bg })
    .expect("CloseTab background");
  worker
    .tx
    .send(UiToWorker::CloseTab { tab_id: tab_active })
    .expect("CloseTab active");

  drop(worker.tx);
  worker.join.join().expect("worker join");
}
