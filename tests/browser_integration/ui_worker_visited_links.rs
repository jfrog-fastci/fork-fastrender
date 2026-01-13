#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, PointerButton, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;
use url::Url;

// This test performs real navigations and paints; keep timeout generous for contended CI hosts.
const TIMEOUT: Duration = Duration::from_secs(20);

fn wait_for_navigation_committed(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
  expected_url: &str,
) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationCommitted { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationCommitted for tab {tab_id:?}"));

  match msg {
    WorkerToUi::NavigationCommitted { url, .. } => {
      assert_eq!(url, expected_url);
    }
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}");
    }
    other => {
      panic!("unexpected WorkerToUi message while waiting for NavigationCommitted: {other:?}")
    }
  }
}

fn wait_for_frame_ready(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
) -> fastrender::ui::messages::RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));
  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("unexpected WorkerToUi message while waiting for FrameReady: {other:?}"),
  }
}

#[test]
fn visited_pseudo_class_persists_across_back_navigation() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();

  let url_a = site.write(
    "a.html",
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body { margin: 0; padding: 0; }
            a {
              position: absolute;
              top: 0;
              left: 0;
              width: 128px;
              height: 128px;
              display: block;
              font-size: 0;
            }
            a:link { background: rgb(255, 0, 0); }
            a:visited { background: rgb(0, 255, 0); }
          </style>
        </head>
        <body>
          <a href="b.html">go</a>
        </body>
      </html>
    "#,
  );

  let url_b = site.write(
    "b.html",
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body { margin: 0; padding: 0; background: rgb(0, 0, 255); }
          </style>
        </head>
        <body></body>
      </html>
    "#,
  );

  let expected_b = Url::parse(&url_a)
    .expect("parse url_a")
    .join("b.html")
    .expect("resolve b.html")
    .to_string();
  assert_eq!(expected_b, url_b);

  let handle = spawn_ui_worker("fastr-ui-worker-visited-links").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (128, 128), 1.0))
    .expect("viewport");

  // Load page A. The link should start unvisited (`:link`).
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url_a.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate a");
  wait_for_navigation_committed(&ui_rx, tab_id, &url_a);
  let frame_a0 = wait_for_frame_ready(&ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame_a0.pixmap, 64, 64),
    [255, 0, 0, 255],
    "expected link to start unvisited (red)"
  );

  // Click the link to navigate to page B.
  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("PointerDown");
  ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("PointerUp");
  wait_for_navigation_committed(&ui_rx, tab_id, &url_b);
  // Drain the paint for page B so the subsequent back navigation doesn't race queued frames.
  let _ = wait_for_frame_ready(&ui_rx, tab_id);

  // Navigate back to A; the link to B should now match `:visited`.
  ui_tx.send(UiToWorker::GoBack { tab_id }).expect("GoBack");
  wait_for_navigation_committed(&ui_rx, tab_id, &url_a);
  let frame_a1 = wait_for_frame_ready(&ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame_a1.pixmap, 64, 64),
    [0, 255, 0, 255],
    "expected link to be visited (green) after navigating back"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
