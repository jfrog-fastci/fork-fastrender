#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{KeyAction, NavigationReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

// Rendering + layout can take a few seconds under CI contention.
const TIMEOUT: Duration = Duration::from_secs(20);

fn fixture() -> (support::TempSite, String) {
  let site = support::TempSite::new();

  let index_url = site.write(
    "index.html",
    r#"<!doctype html><html><head><meta charset="utf-8"><style>
html, body { margin: 0; padding: 0; }
a { display: block; width: 200px; height: 80px; background: #eee; }
</style></head><body><a href="https://example.com/">Example link</a></body></html>"#,
  );

  (site, index_url)
}

#[test]
fn a11y_show_context_menu_on_focused_link_reports_link_url() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let (_site, index_url) = fixture();

  let worker =
    spawn_ui_worker("fastr-ui-worker-accesskit-show-context-menu").expect("spawn ui worker");
  let tab_id = TabId(1);

  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (320, 240), 1.0))
    .unwrap();
  worker
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      index_url,
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  // Wait for the first paint so the worker has layout artifacts for computing node bounds.
  let _frame_msg = support::recv_for_tab(&worker.ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for initial FrameReady for tab {tab_id:?}"));

  // Focus the link via keyboard traversal (do not click, which would navigate).
  worker
    .ui_tx
    .send(support::key_action(tab_id, KeyAction::Tab))
    .unwrap();

  // Simulate an assistive-technology "Show context menu" action.
  worker
    .ui_tx
    .send(UiToWorker::A11yShowContextMenu {
      tab_id,
      node_id: None,
    })
    .unwrap();

  let msg = support::recv_for_tab(&worker.ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ContextMenu { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for ContextMenu for tab {tab_id:?}"));

  match msg {
    WorkerToUi::ContextMenu { link_url, .. } => {
      assert_eq!(link_url.as_deref(), Some("https://example.com/"));
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  worker.join().unwrap();
}
