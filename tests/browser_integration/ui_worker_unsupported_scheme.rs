#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

use super::support::{
  create_tab_msg, drain_for, format_messages, navigate_msg, viewport_changed_msg, DEFAULT_TIMEOUT,
};

#[test]
fn ui_worker_rejects_unsupported_schemes_without_rendering_error_page() {
  let _lock = super::stage_listener_test_lock();

  let (ui_tx, ui_rx, join) = spawn_ui_worker("fastr-ui-worker-unsupported-scheme-test")
    .expect("spawn ui worker")
    .split();

  let tab_id = TabId::new();
  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("viewport");

  let url = "javascript:alert(1)".to_string();
  ui_tx
    .send(navigate_msg(
      tab_id,
      url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate");

  let Some(msg) = super::support::recv_for_tab(
    &ui_rx,
    tab_id,
    DEFAULT_TIMEOUT,
    |msg| matches!(msg, WorkerToUi::NavigationFailed { url: failed, .. } if failed == &url),
  ) else {
    panic!("timed out waiting for NavigationFailed for {url}");
  };

  let WorkerToUi::NavigationFailed { error, .. } = msg else {
    unreachable!();
  };
  assert!(
    error.to_ascii_lowercase().contains("javascript"),
    "expected error to mention the javascript scheme; got: {error}"
  );

  // Unsupported URL schemes should fail fast without rendering an `about:error` fallback page.
  let drained = drain_for(&ui_rx, Duration::from_millis(200));
  assert!(
    !drained.iter().any(
      |msg| matches!(msg, WorkerToUi::FrameReady { tab_id: msg_tab, .. } if *msg_tab == tab_id)
    ),
    "expected no FrameReady after unsupported-scheme navigation; got:\n{}",
    format_messages(&drained)
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
