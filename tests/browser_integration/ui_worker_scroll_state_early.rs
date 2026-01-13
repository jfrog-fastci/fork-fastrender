#![cfg(feature = "browser_ui")]

use fastrender::ui::cancel::CancelGens;
use fastrender::ui::{spawn_browser_worker_for_test, NavigationReason, TabId, WorkerToUi};
use tempfile::tempdir;

use super::support::{
  create_tab_msg_with_cancel, navigate_msg, scroll_msg, viewport_changed_msg,
  wait_for_frame_and_scroll_state_updated, DEFAULT_TIMEOUT,
};

#[test]
fn scroll_state_updated_matches_frame_ready_with_scroll_snap() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let dir = tempdir().expect("temp dir");
  let path = dir.path().join("scroll.html");
  std::fs::write(
    &path,
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            html { scroll-snap-type: y mandatory; }
            .snap { height: 100px; scroll-snap-align: start; }
          </style>
        </head>
        <body>
          <div class="snap" style="background: rgb(255,0,0)"></div>
          <div class="snap" style="background: rgb(0,0,255)"></div>
          <div class="snap" style="background: rgb(0,255,0)"></div>
        </body>
      </html>
    "#,
  )
  .expect("write html");

  let url = url::Url::from_file_path(&path)
    .expect("file URL")
    .to_string();

  // Make paints slow so we exercise the "scroll update is computed without needing the paint to
  // complete" code path. The test does not assume ordering between `ScrollStateUpdated` and
  // `FrameReady`.
  let fastrender::ui::BrowserWorkerHandle { tx, rx, join } =
    spawn_browser_worker_for_test(Some(200)).expect("spawn browser worker");

  let tab_id = TabId(1);
  let cancel = CancelGens::new();

  tx.send(create_tab_msg_with_cancel(tab_id, None, cancel))
    .expect("CreateTab");
  tx.send(viewport_changed_msg(tab_id, (100, 100), 1.0))
    .expect("ViewportChanged");
  tx.send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  // Wait for the initial frame so cached layout is available for scroll snap.
  super::support::recv_for_tab(&rx, tab_id, DEFAULT_TIMEOUT * 2, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("initial FrameReady");

  // Ensure the channel is quiet before issuing the scroll request.
  while rx.try_recv().is_ok() {}

  tx.send(scroll_msg(tab_id, (0.0, 60.0), None))
    .expect("Scroll");

  let (frame, scroll) = wait_for_frame_and_scroll_state_updated(&rx, tab_id, DEFAULT_TIMEOUT);
  assert!(
    (scroll.viewport.y - 100.0).abs() < 1.0,
    "expected scroll snap to land at ~100px, got {:?}",
    scroll.viewport
  );

  assert_eq!(
    frame.scroll_state, scroll,
    "expected ScrollStateUpdated to match FrameReady.scroll_state"
  );

  drop(tx);
  join.join().unwrap();
}
