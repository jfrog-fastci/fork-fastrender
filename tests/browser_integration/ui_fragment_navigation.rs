#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, PointerButton, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::worker::spawn_ui_worker;
use std::time::{Duration, Instant};
use tempfile::tempdir;
use url::Url;

fn recv_until_frame(rx: &std::sync::mpsc::Receiver<WorkerToUi>, tab_id: TabId, deadline: Instant) -> fastrender::ui::messages::RenderedFrame {
  loop {
    let now = Instant::now();
    if now >= deadline {
      panic!("timed out waiting for FrameReady");
    }
    let remaining = deadline.saturating_duration_since(now);
    match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(msg) => match msg {
        WorkerToUi::FrameReady { tab_id: msg_tab, frame } if msg_tab == tab_id => return frame,
        _ => {}
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
        panic!("worker channel disconnected while waiting for FrameReady");
      }
    }
  }
}

#[test]
fn same_document_fragment_navigation_scrolls_without_fetching() {
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let html_path = dir.path().join("page.html");
  let html = r##"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #link { display: block; width: 120px; height: 40px; background: rgb(255, 0, 0); }
          #spacer { height: 2000px; }
        </style>
      </head>
      <body>
        <a id="link" href="#target">Jump</a>
        <div id="spacer"></div>
        <div id="target">Target</div>
      </body>
    </html>
  "##;
  std::fs::write(&html_path, html).expect("write html");
  let file_url = Url::from_file_path(&html_path)
    .unwrap_or_else(|()| panic!("failed to build file:// url for {}", html_path.display()))
    .to_string();

  let worker = spawn_ui_worker("fastr-ui-worker-fragment-nav").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 120), 1.0))
    .unwrap();
  worker
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      file_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  let deadline = Instant::now() + Duration::from_secs(15);
  let initial_frame = recv_until_frame(&worker.ui_rx, tab_id, deadline);
  let initial_scroll_y = initial_frame.scroll_state.viewport.y;

  // Drain any remaining messages from the initial navigation so we only observe the fragment nav.
  while worker.ui_rx.try_recv().is_ok() {}

  worker
    .ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .unwrap();
  worker
    .ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .unwrap();

  let deadline = Instant::now() + Duration::from_secs(15);
  let mut saw_started = false;
  let mut saw_failed = false;
  let mut committed = None;
  let mut scroll_after_commit = None;
  while Instant::now() < deadline {
    match worker.ui_rx.recv_timeout(Duration::from_millis(200)) {
      Ok(msg) => match msg {
        WorkerToUi::NavigationStarted { .. } => saw_started = true,
        WorkerToUi::NavigationFailed { .. } => saw_failed = true,
        WorkerToUi::NavigationCommitted { url, can_go_back, .. } => {
          if url.ends_with("#target") {
            committed = Some((url, can_go_back));
          }
        }
        WorkerToUi::ScrollStateUpdated { scroll, .. } => {
          if committed.is_some() {
            scroll_after_commit = Some(scroll.viewport.y);
          }
        }
        WorkerToUi::FrameReady { frame, .. } => {
          if committed.is_some() {
            scroll_after_commit = Some(frame.scroll_state.viewport.y);
            break;
          }
        }
        _ => {}
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  assert!(
    !saw_started,
    "expected fragment navigation to avoid NavigationStarted"
  );
  assert!(
    !saw_failed,
    "expected fragment navigation to avoid NavigationFailed"
  );

  let (committed_url, can_go_back) = committed.expect("expected NavigationCommitted for fragment");
  assert!(
    can_go_back,
    "expected fragment navigation to push history (can_go_back should become true)"
  );
  assert!(
    committed_url.ends_with("#target"),
    "expected committed URL to include fragment, got {committed_url:?}"
  );

  let new_scroll_y = scroll_after_commit.expect("expected scroll update after commit");
  assert!(
    new_scroll_y > initial_scroll_y,
    "expected fragment navigation to increase scroll y (initial={initial_scroll_y}, new={new_scroll_y})"
  );

  worker.join().unwrap();
}
