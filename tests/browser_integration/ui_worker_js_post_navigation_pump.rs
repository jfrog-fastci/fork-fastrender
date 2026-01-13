#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

// Real navigations + paints can be slow on contended CI hosts.
const TIMEOUT: Duration = Duration::from_secs(20);

fn wait_for_navigation_committed(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
  url: &str,
) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationCommitted { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationCommitted for tab {tab_id:?}"));

  match msg {
    WorkerToUi::NavigationCommitted { url: committed, .. } => {
      assert_eq!(committed, url);
    }
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected message while waiting for NavigationCommitted: {other:?}"),
  }
}

fn wait_for_frame_ready(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
) -> fastrender::ui::messages::RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| matches!(msg, WorkerToUi::FrameReady { .. }))
    .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));
  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("unexpected message while waiting for FrameReady: {other:?}"),
  }
}

#[test]
fn ui_worker_pumps_js_after_navigation_commit_and_repaints_on_dom_changes() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body { margin: 0; padding: 0; width: 100%; height: 100%; background: rgb(255, 0, 0); }
          </style>
          <script>
            // This mutation runs from the DOMContentLoaded task queued after parsing.
            document.addEventListener('DOMContentLoaded', () => {
              document.documentElement.style.background = 'rgb(0, 255, 0)';
              document.body.style.background = 'rgb(0, 255, 0)';
            });
          </script>
        </head>
        <body></body>
      </html>
    "#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-js-post-nav-pump").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .expect("viewport");

  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate");

  wait_for_navigation_committed(&ui_rx, tab_id, &url);
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame.pixmap, 32, 32),
    [0, 255, 0, 255],
    "expected DOMContentLoaded mutation to be visible in the first rendered frame after navigation"
  );

  // Exercise the worker shutdown path used by other UI integration tests.
  ui_tx
    .send(UiToWorker::CloseTab { tab_id })
    .expect("close tab");
  drop(ui_tx);
  join.join().expect("join ui worker");
}
