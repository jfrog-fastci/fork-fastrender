#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::scroll::ScrollState;
use fastrender::ui::messages::WorkerToUi;
use fastrender::ui::worker::spawn_ui_worker;
use fastrender::ui::{NavigationReason, TabId, UiToWorker};
use std::sync::mpsc::Receiver;
use std::time::Duration;

// Navigation/rendering can take a few seconds under load when tests run in parallel (CI).
const TIMEOUT: Duration = Duration::from_secs(20);

fn next_navigation_committed(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> (String, bool, bool) {
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

fn next_scroll_state_updated(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> ScrollState {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ScrollStateUpdated { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for ScrollStateUpdated for tab {tab_id:?}"));

  match msg {
    WorkerToUi::ScrollStateUpdated { scroll, .. } => scroll,
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
  let site = support::TempSite::new();
  let url_a = site.write("a.html", &simple_color_page("rgb(255, 0, 0)"));
  let url_b = site.write("b.html", &simple_color_page("rgb(0, 0, 255)"));

  let handle = spawn_ui_worker("fastr-ui-worker-nav-messages").expect("spawn ui worker");
  let tab_id = TabId::new();

  handle
    .ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: Default::default(),
    })
    .expect("create tab");
  handle
    .ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (64, 64),
      dpr: 1.0,
    })
    .expect("viewport");

  handle
    .ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: url_a.clone(),
      reason: NavigationReason::TypedUrl,
    })
    .expect("navigate a");
  let (url, back, forward) = next_navigation_committed(&handle.ui_rx, tab_id);
  assert_eq!(url, url_a);
  assert!(!back);
  assert!(!forward);
  let scroll = next_scroll_state_updated(&handle.ui_rx, tab_id);
  assert_eq!(scroll.viewport.y, 0.0);

  handle
    .ui_tx
    .send(UiToWorker::Scroll {
      tab_id,
      delta_css: (0.0, 120.0),
      pointer_css: None,
    })
    .expect("scroll a");
  let scroll = next_scroll_state_updated(&handle.ui_rx, tab_id);
  assert_eq!(scroll.viewport.y, 120.0);

  handle
    .ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: url_b.clone(),
      reason: NavigationReason::TypedUrl,
    })
    .expect("navigate b");
  let (url, back, forward) = next_navigation_committed(&handle.ui_rx, tab_id);
  assert_eq!(url, url_b);
  assert!(back);
  assert!(!forward);
  let scroll = next_scroll_state_updated(&handle.ui_rx, tab_id);
  assert_eq!(scroll.viewport.y, 0.0);

  handle
    .ui_tx
    .send(UiToWorker::Scroll {
      tab_id,
      delta_css: (0.0, 240.0),
      pointer_css: None,
    })
    .expect("scroll b");
  let scroll = next_scroll_state_updated(&handle.ui_rx, tab_id);
  assert_eq!(scroll.viewport.y, 240.0);

  handle
    .ui_tx
    .send(UiToWorker::GoBack { tab_id })
    .expect("go back");
  let (url, back, forward) = next_navigation_committed(&handle.ui_rx, tab_id);
  assert_eq!(url, url_a);
  assert!(!back);
  assert!(forward);
  let scroll = next_scroll_state_updated(&handle.ui_rx, tab_id);
  assert_eq!(scroll.viewport.y, 120.0);

  handle
    .ui_tx
    .send(UiToWorker::GoForward { tab_id })
    .expect("go forward");
  let (url, back, forward) = next_navigation_committed(&handle.ui_rx, tab_id);
  assert_eq!(url, url_b);
  assert!(back);
  assert!(!forward);
  let scroll = next_scroll_state_updated(&handle.ui_rx, tab_id);
  assert_eq!(scroll.viewport.y, 240.0);

  handle
    .ui_tx
    .send(UiToWorker::Reload { tab_id })
    .expect("reload");
  let (url, back, forward) = next_navigation_committed(&handle.ui_rx, tab_id);
  assert_eq!(url, url_b);
  assert!(back);
  assert!(!forward);
  let scroll = next_scroll_state_updated(&handle.ui_rx, tab_id);
  assert_eq!(scroll.viewport.y, 240.0);

  handle.join().expect("worker join");
}
