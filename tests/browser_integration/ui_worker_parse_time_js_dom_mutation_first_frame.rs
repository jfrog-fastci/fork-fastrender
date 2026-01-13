#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::time::Duration;

// Worker startup + navigation + rendering can take a few seconds under load when integration tests
// run in parallel on CI; keep this timeout generous to avoid flakiness.
const TIMEOUT: Duration = Duration::from_secs(20);

#[test]
fn ui_worker_first_frame_reflects_parse_time_js_dom_mutation() {
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
      html, body { margin: 0; padding: 0; }
      #box { width: 64px; height: 64px; background: rgb(255, 0, 0); }
    </style>
  </head>
  <body>
    <div id="box"></div>
    <script>
      // The inline script runs during HTML parsing, so the first rendered frame should include the
      // style mutation below.
      document.getElementById("box").style.background = "rgb(0, 0, 255)";
    </script>
  </body>
</html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-parse-time-js",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, Some(url.clone())))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .expect("viewport");

  let msg = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {url}"));

  let frame = match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}")
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  };

  let x = frame.pixmap.width() / 2;
  let y = frame.pixmap.height() / 2;
  let rgba = support::rgba_at(&frame.pixmap, x, y);
  assert_eq!(
    rgba,
    [0, 0, 255, 255],
    "expected first painted frame to include parse-time JS DOM/style mutation"
  );

  drop(ui_tx);
  join.join().expect("worker join");
}

