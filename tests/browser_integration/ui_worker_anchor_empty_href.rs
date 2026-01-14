#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  KeyAction, NavigationReason, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn next_frame_ready(rx: &Receiver<WorkerToUi>, tab_id: TabId) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  if let WorkerToUi::NavigationFailed { url, error, .. } = msg {
    panic!("navigation failed for {url}: {error}");
  }
}

fn next_navigation_started(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> String {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationStarted { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationStarted for tab {tab_id:?}"));
  match msg {
    WorkerToUi::NavigationStarted { url, .. } => url,
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn next_navigation_committed(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> String {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationCommitted { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationCommitted for tab {tab_id:?}"));
  match msg {
    WorkerToUi::NavigationCommitted { url, .. } => url,
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn next_hovered_url(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> Option<String> {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::HoverChanged { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for HoverChanged for tab {tab_id:?}"));
  match msg {
    WorkerToUi::HoverChanged { hovered_url, .. } => hovered_url,
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn next_context_menu_link_url(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> Option<String> {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ContextMenu { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for ContextMenu for tab {tab_id:?}"));
  match msg {
    WorkerToUi::ContextMenu { link_url, .. } => link_url,
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

#[test]
fn anchor_with_empty_href_is_focusable_and_navigable() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "page.html",
    r##"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #link {
              position: absolute;
              left: 0;
              top: 0;
              display: block;
              width: 120px;
              height: 40px;
              background: rgb(255, 0, 0);
            }
          </style>
        </head>
        <body>
          <a id="link" href="">Reload</a>
        </body>
      </html>
    "##,
  );

  let worker = spawn_ui_worker("fastr-ui-worker-empty-href").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("create tab");
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("viewport");
  worker
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate");

  // Wait for an initial frame so the tab is fully loaded.
  next_frame_ready(&worker.ui_rx, tab_id);
  let _ = support::drain_for(&worker.ui_rx, Duration::from_millis(50));

  // ---------------------------------------------------------------------------
  // Keyboard activation: Tab should be able to focus `<a href=\"\">`, then Enter should navigate.
  // ---------------------------------------------------------------------------
  worker
    .ui_tx
    .send(support::key_action(tab_id, KeyAction::Tab))
    .expect("tab key");
  worker
    .ui_tx
    .send(support::key_action(tab_id, KeyAction::Enter))
    .expect("enter key");

  let started = next_navigation_started(&worker.ui_rx, tab_id);
  assert_eq!(started, url, "expected Tab+Enter to navigate to the same URL");
  let committed = next_navigation_committed(&worker.ui_rx, tab_id);
  assert_eq!(committed, url, "expected Tab+Enter to commit the same URL");
  next_frame_ready(&worker.ui_rx, tab_id);
  let _ = support::drain_for(&worker.ui_rx, Duration::from_millis(50));

  // ---------------------------------------------------------------------------
  // Hover + context menu: empty href should still resolve to the current URL for UI metadata.
  // ---------------------------------------------------------------------------
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("pointer move");
  let hovered = next_hovered_url(&worker.ui_rx, tab_id);
  assert_eq!(
    hovered.as_deref(),
    Some(url.as_str()),
    "expected hover to resolve empty href to the base URL"
  );

  worker
    .ui_tx
    .send(UiToWorker::ContextMenuRequest {
      tab_id,
      pos_css: (10.0, 10.0),
      modifiers: PointerModifiers::NONE,
    })
    .expect("context menu request");
  let link_url = next_context_menu_link_url(&worker.ui_rx, tab_id);
  assert_eq!(
    link_url.as_deref(),
    Some(url.as_str()),
    "expected context menu link_url to resolve empty href to the base URL"
  );
  let _ = support::drain_for(&worker.ui_rx, Duration::from_millis(50));

  // ---------------------------------------------------------------------------
  // Pointer activation: clicking `<a href=\"\">` should also navigate.
  // ---------------------------------------------------------------------------
  worker
    .ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("pointer down");
  worker
    .ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("pointer up");

  let started = next_navigation_started(&worker.ui_rx, tab_id);
  assert_eq!(started, url, "expected click to navigate to the same URL");
  let committed = next_navigation_committed(&worker.ui_rx, tab_id);
  assert_eq!(committed, url, "expected click to commit the same URL");
  next_frame_ready(&worker.ui_rx, tab_id);

  worker.join().expect("worker join");
}
