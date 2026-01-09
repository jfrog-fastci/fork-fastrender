#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::worker::spawn_ui_worker;
use std::time::Duration;

// Startup + first paint can take several seconds under load when browser integration tests run in
// parallel (default `cargo test` behavior). Keep this timeout generous to avoid flakiness on busy
// CI hosts.
const TIMEOUT: Duration = Duration::from_secs(30);

#[test]
fn navigation_with_percent_encoded_fragment_scrolls_and_targets_unicode_id() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page_url = site.write(
    "page.html",
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body { margin: 0; padding: 0; }
            .spacer { height: 2000px; }
            [id="café"] { height: 40px; background: rgb(255, 0, 0); }
            [id="café"]:target { background: rgb(0, 255, 0); }
          </style>
        </head>
        <body>
          <div class="spacer"></div>
          <div id="café"></div>
          <div class="spacer"></div>
        </body>
      </html>
    "#,
  );
  let url = format!("{page_url}#caf%C3%A9");

  let worker =
    spawn_ui_worker("fastr-ui-worker-anchor-scroll-percent-encoded").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("create tab");
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("viewport");
  worker
    .ui_tx
    .send(support::navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("navigate");

  let msg = support::recv_for_tab(&worker.ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationCommitted { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .expect("NavigationCommitted");
  match msg {
    WorkerToUi::NavigationCommitted { .. } => {}
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  let msg = support::recv_for_tab(&worker.ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("FrameReady");
  let frame = match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  };

  assert!(
    frame.scroll_state.viewport.y > 0.0,
    "expected fragment navigation to scroll viewport; got {:?}",
    frame.scroll_state.viewport
  );
  assert_eq!(
    support::rgba_at(&frame.pixmap, 10, 10),
    [0, 255, 0, 255],
    "expected :target styling + scroll to bring the green target into view"
  );

  worker.join().expect("join ui worker");
}
