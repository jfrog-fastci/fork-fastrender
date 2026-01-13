#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::time::Duration;

#[test]
fn ui_worker_lazy_session_restore_defers_background_tab_navigation_until_activated() {
  let _lock = super::stage_listener_test_lock();

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-lazy-session-restore-test",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");

  let tab_a = TabId::new();
  let tab_b = TabId::new();

  handle
    .ui_tx
    .send(support::create_tab_msg(tab_a, None))
    .expect("CreateTab A");
  handle
    .ui_tx
    .send(support::create_tab_msg(tab_b, None))
    .expect("CreateTab B");

  // Load only the initially active tab.
  handle
    .ui_tx
    .send(support::viewport_changed_msg(tab_a, (200, 120), 1.0))
    .expect("ViewportChanged A");
  handle
    .ui_tx
    .send(UiToWorker::SetActiveTab { tab_id: tab_a })
    .expect("SetActiveTab A");
  handle
    .ui_tx
    .send(support::navigate_msg(
      tab_a,
      "about:blank".to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("Navigate A");

  let msg = support::recv_for_tab(&handle.ui_rx, tab_a, support::DEFAULT_TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationCommitted { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationCommitted for tab {tab_a:?}"));
  if let WorkerToUi::NavigationFailed { url, error, .. } = msg {
    panic!("navigation failed for {url}: {error}");
  }

  // Background tabs should not navigate until activated.
  let unexpected = support::recv_for_tab(&handle.ui_rx, tab_b, Duration::from_millis(200), |msg| {
    matches!(msg, WorkerToUi::NavigationCommitted { .. })
  });
  assert!(
    unexpected.is_none(),
    "unexpected NavigationCommitted for background tab before activation: {unexpected:?}"
  );

  // Activate the background tab and navigate.
  handle
    .ui_tx
    .send(support::viewport_changed_msg(tab_b, (200, 120), 1.0))
    .expect("ViewportChanged B");
  handle
    .ui_tx
    .send(UiToWorker::SetActiveTab { tab_id: tab_b })
    .expect("SetActiveTab B");
  handle
    .ui_tx
    .send(support::navigate_msg(
      tab_b,
      "about:newtab".to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("Navigate B");

  let msg = support::recv_for_tab(&handle.ui_rx, tab_b, support::DEFAULT_TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationCommitted { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationCommitted for tab {tab_b:?}"));
  if let WorkerToUi::NavigationFailed { url, error, .. } = msg {
    panic!("navigation failed for {url}: {error}");
  }

  drop(handle.ui_tx);
  drop(handle.ui_rx);
  handle.join.join().expect("worker join");
}
