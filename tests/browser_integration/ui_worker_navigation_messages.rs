#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::WorkerToUi;
use fastrender::ui::spawn_ui_worker;
use fastrender::ui::{NavigationReason, TabId, UiToWorker};
use std::time::Duration;

// Worker startup + navigation + rendering can take a few seconds under load when integration tests
// run in parallel on CI; keep this timeout generous to avoid flakiness.
const TIMEOUT: Duration = Duration::from_secs(20);

fn next_navigation_committed(
  rx: &fastrender::ui::WorkerToUiInbox,
  tab_id: TabId,
) -> (String, bool, bool) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationCommitted { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationCommitted for tab {tab_id:?}"));

  match msg {
    WorkerToUi::NavigationCommitted {
      url,
      can_go_back,
      can_go_forward,
      ..
    } => (url, can_go_back, can_go_forward),
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn next_scroll_state(
  rx: &fastrender::ui::WorkerToUiInbox,
  tab_id: TabId,
) -> fastrender::scroll::ScrollState {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame.scroll_state,
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn simple_color_page(color: &str) -> String {
  format!(
    r#"<!doctype html>
<html>
  <head>
    <style>
      html, body {{ margin: 0; padding: 0; }}
      body {{ background: {color}; }}
      #spacer {{ height: 2000px; }}
    </style>
  </head>
  <body>
    <div id="spacer"></div>
  </body>
</html>"#
  )
}

#[test]
fn history_navigation_messages_update_history_and_restore_scroll() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let site = support::TempSite::new();
  let url_a = site.write("a.html", &simple_color_page("rgb(255, 0, 0)"));
  let url_b = site.write("b.html", &simple_color_page("rgb(0, 0, 255)"));

  let handle = spawn_ui_worker("fastr-ui-worker-nav-messages").expect("spawn ui worker");
  let tab_id = TabId::new();

  handle
    .ui_tx
    .send(support::create_tab_msg(
      tab_id,
      Some("about:newtab".to_string()),
    ))
    .expect("create tab");
  handle
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .expect("viewport");

  // Drain the initial about:newtab navigation so subsequent assertions don't race it.
  let (url, back, forward) = next_navigation_committed(&handle.ui_rx, tab_id);
  assert_eq!(url, "about:newtab");
  assert!(!back);
  assert!(!forward);
  let _ = next_scroll_state(&handle.ui_rx, tab_id);
  while handle.ui_rx.try_recv().is_ok() {}

  handle
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      url_a.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate a");
  let (url, back, forward) = next_navigation_committed(&handle.ui_rx, tab_id);
  assert_eq!(url, url_a);
  assert!(back);
  assert!(!forward);
  let scroll = next_scroll_state(&handle.ui_rx, tab_id);
  assert_eq!(scroll.viewport.y, 0.0);

  handle
    .ui_tx
    .send(support::scroll_msg(tab_id, (0.0, 120.0), None))
    .expect("scroll a");
  let scroll = next_scroll_state(&handle.ui_rx, tab_id);
  assert_eq!(scroll.viewport.y, 120.0);

  handle
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      url_b.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate b");
  let (url, back, forward) = next_navigation_committed(&handle.ui_rx, tab_id);
  assert_eq!(url, url_b);
  assert!(back);
  assert!(!forward);
  let scroll = next_scroll_state(&handle.ui_rx, tab_id);
  assert_eq!(scroll.viewport.y, 0.0);

  handle
    .ui_tx
    .send(support::scroll_msg(tab_id, (0.0, 240.0), None))
    .expect("scroll b");
  let scroll = next_scroll_state(&handle.ui_rx, tab_id);
  assert_eq!(scroll.viewport.y, 240.0);

  handle
    .ui_tx
    .send(UiToWorker::GoBack { tab_id })
    .expect("go back");
  let (url, back, forward) = next_navigation_committed(&handle.ui_rx, tab_id);
  assert_eq!(url, url_a);
  assert!(back);
  assert!(forward);
  let scroll = next_scroll_state(&handle.ui_rx, tab_id);
  assert_eq!(scroll.viewport.y, 120.0);

  handle
    .ui_tx
    .send(UiToWorker::GoForward { tab_id })
    .expect("go forward");
  let (url, back, forward) = next_navigation_committed(&handle.ui_rx, tab_id);
  assert_eq!(url, url_b);
  assert!(back);
  assert!(!forward);
  let scroll = next_scroll_state(&handle.ui_rx, tab_id);
  assert_eq!(scroll.viewport.y, 240.0);

  handle
    .ui_tx
    .send(UiToWorker::Reload { tab_id })
    .expect("reload");
  let (url, back, forward) = next_navigation_committed(&handle.ui_rx, tab_id);
  assert_eq!(url, url_b);
  assert!(back);
  assert!(!forward);
  let scroll = next_scroll_state(&handle.ui_rx, tab_id);
  assert_eq!(scroll.viewport.y, 240.0);

  handle.join().expect("worker join");
}
