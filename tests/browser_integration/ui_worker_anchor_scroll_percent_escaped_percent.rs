#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::worker::spawn_ui_worker;
use std::time::Duration;

// Keep this generous since browser UI integration tests can run in parallel.
const TIMEOUT: Duration = Duration::from_secs(30);

#[test]
fn navigation_with_percent_encoded_percent_fragment_scrolls_to_target() {
  let _lock = super::stage_listener_test_lock();

  // Regression test for double-decoding fragment identifiers:
  // - HTML id is the literal string "%23foo"
  // - URL fragment is "%2523foo" (decodes once to "%23foo")
  //
  // `scroll_offset_for_fragment_target` percent-decodes internally, so call sites must pass the
  // *raw* (still percent-encoded) fragment string. If a call site decodes first, we decode twice
  // ("%2523foo" -> "%23foo" -> "#foo") and fail to find the target.
  let site = support::TempSite::new();
  let page_url = site.write(
    "page.html",
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body { margin: 0; padding: 0; }
            .spacer { height: 2000px; background: rgb(0, 0, 255); }
            [id="%23foo"] { height: 40px; background: rgb(255, 0, 0); }
            [id="%23foo"]:target { background: rgb(0, 255, 0); }
          </style>
        </head>
        <body>
          <div class="spacer"></div>
          <div id="%23foo"></div>
          <div class="spacer"></div>
        </body>
      </html>
    "#,
  );
  let url = format!("{page_url}#%2523foo");

  let worker = spawn_ui_worker("fastr-ui-worker-anchor-scroll-percent-escaped-percent")
    .expect("spawn ui worker");
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

