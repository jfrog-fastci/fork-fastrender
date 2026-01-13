#![cfg(feature = "browser_ui")]

use fastrender::ui::cancel::CancelGens;
use fastrender::ui::{spawn_browser_worker_for_test, NavigationReason, TabId, WorkerToUi};
use std::time::{Duration, Instant};
use tempfile::tempdir;

use super::support::{
  create_tab_msg_with_cancel, format_messages, navigate_msg, scroll_msg, viewport_changed_msg,
  DEFAULT_TIMEOUT,
};

#[test]
fn scroll_state_updated_is_emitted_before_frame_ready_with_scroll_snap() {
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

  // Make paints slow so the test can observe early scroll updates without racing a FrameReady.
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

  // Drain the initial ScrollStateUpdated from navigation so subsequent waits don't match it.
  let _ = super::support::recv_for_tab(&rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ScrollStateUpdated { .. })
  });

  // Ensure the channel is quiet before issuing the scroll request.
  while rx.try_recv().is_ok() {}

  tx.send(scroll_msg(tab_id, (0.0, 60.0), None))
    .expect("Scroll");

  let start = Instant::now();
  let mut seen: Vec<WorkerToUi> = Vec::new();
  let mut early_scroll: Option<fastrender::scroll::ScrollState> = None;
  let mut early_elapsed: Option<Duration> = None;

  // Expect an early ScrollStateUpdated long before the paint finishes.
  while start.elapsed() < Duration::from_millis(50) {
    let remaining = Duration::from_millis(50).saturating_sub(start.elapsed());
    match rx.recv_timeout(remaining) {
      Ok(msg) => {
        match msg {
          WorkerToUi::ScrollStateUpdated { tab_id: got, scroll } if got == tab_id => {
            if scroll.viewport.y > 0.0 {
              early_scroll = Some(scroll);
              early_elapsed = Some(start.elapsed());
              break;
            }
            if seen.len() < 64 {
              seen.push(WorkerToUi::ScrollStateUpdated { tab_id: got, scroll });
            }
          }
          WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => {
            // If a frame arrives first, the worker isn't emitting early scroll updates.
            if seen.len() < 64 {
              seen.push(WorkerToUi::FrameReady { tab_id: got, frame });
            }
            panic!(
              "expected early ScrollStateUpdated before FrameReady\nmessages:\n{}",
              format_messages(&seen)
            );
          }
          other => {
            if seen.len() < 64 {
              seen.push(other);
            }
          }
        }
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  let early_scroll = early_scroll.unwrap_or_else(|| {
    panic!(
      "timed out waiting for early ScrollStateUpdated\nmessages:\n{}",
      format_messages(&seen)
    )
  });
  let early_elapsed = early_elapsed.unwrap_or_else(|| Duration::from_millis(50));

  assert!(
    early_elapsed < Duration::from_millis(50),
    "expected early ScrollStateUpdated within 50ms, got {:?}",
    early_elapsed
  );

  assert!(
    (early_scroll.viewport.y - 100.0).abs() < 1.0,
    "expected scroll snap to land at ~100px, got {:?}",
    early_scroll.viewport
  );

  // The subsequent FrameReady must reflect the same snapped/clamped scroll state.
  let frame = super::support::recv_for_tab(&rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .and_then(|msg| match msg {
    WorkerToUi::FrameReady { frame, .. } => Some(frame),
    _ => None,
  })
  .expect("FrameReady after scroll");

  assert_eq!(
    frame.scroll_state, early_scroll,
    "expected early ScrollStateUpdated to match subsequent FrameReady.scroll_state"
  );

  drop(tx);
  join.join().unwrap();
}
