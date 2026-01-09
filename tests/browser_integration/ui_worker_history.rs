#![cfg(feature = "browser_ui")]

use fastrender::scroll::ScrollState;
use fastrender::ui::messages::WorkerToUi;
use fastrender::ui::worker::spawn_ui_worker;
use fastrender::ui::{NavigationReason, RenderedFrame, TabId, UiToWorker};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};
use tempfile::tempdir;
use url::Url;

use super::support::{create_tab_msg, navigate_msg, scroll_msg, viewport_changed_msg};

// These tests run alongside other render-heavy integration tests; allow extra slack to avoid
// flakes under CPU contention.
const TIMEOUT: Duration = Duration::from_secs(15);

fn assert_frame_has_color(frame: &RenderedFrame, expected: (u8, u8, u8, u8)) {
  let pixmap = &frame.pixmap;
  assert!(pixmap.width() > 0 && pixmap.height() > 0, "expected non-empty pixmap");

  // Avoid sampling the right/bottom edges: viewport scrollbars may reserve/paint over them.
  let x1 = if pixmap.width() > 1 { 1 } else { 0 };
  let y1 = if pixmap.height() > 1 { 1 } else { 0 };
  let samples = [
    (x1, y1),
    (pixmap.width() / 2, pixmap.height() / 2),
    (x1, pixmap.height() / 2),
    (pixmap.width() / 2, y1),
  ];

  for (idx, (x, y)) in samples.into_iter().enumerate() {
    let sample = pixmap
      .pixel(x, y)
      .map(|p| (p.red(), p.green(), p.blue(), p.alpha()))
      .unwrap_or((0, 0, 0, 0));
    assert_eq!(
      sample, expected,
      "expected sample {idx} to match {expected:?}; got {sample:?}"
    );
  }
}

fn recv_for_tab<T>(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  timeout: Duration,
  mut map: impl FnMut(WorkerToUi) -> Option<T>,
) -> T {
  let deadline = Instant::now() + timeout;
  loop {
    let now = Instant::now();
    if now >= deadline {
      panic!("timed out waiting for message for tab {tab_id:?}");
    }
    let remaining = deadline.saturating_duration_since(now);
    match rx.recv_timeout(remaining) {
      Ok(msg) => {
        if let Some(value) = map(msg) {
          return value;
        }
      }
      Err(err) => panic!("timed out waiting for tab {tab_id:?}: {err:?}"),
    }
  }
}

fn next_navigation_committed(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> (String, bool, bool) {
  recv_for_tab(rx, tab_id, TIMEOUT, |msg| match msg {
    WorkerToUi::NavigationCommitted {
      tab_id: msg_tab,
      url,
      can_go_back,
      can_go_forward,
      ..
    } if msg_tab == tab_id => Some((url, can_go_back, can_go_forward)),
    WorkerToUi::NavigationFailed {
      tab_id: msg_tab,
      url,
      error,
      ..
    } if msg_tab == tab_id => {
      panic!("navigation failed for {url}: {error}");
    }
    _ => None,
  })
}

fn next_frame_ready(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> RenderedFrame {
  recv_for_tab(rx, tab_id, TIMEOUT, |msg| match msg {
    WorkerToUi::FrameReady {
      tab_id: msg_tab,
      frame,
    } if msg_tab == tab_id => Some(frame),
    _ => None,
  })
}

fn next_scroll_state_updated(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> ScrollState {
  recv_for_tab(rx, tab_id, TIMEOUT, |msg| match msg {
    WorkerToUi::ScrollStateUpdated {
      tab_id: msg_tab,
      scroll,
    } if msg_tab == tab_id => Some(scroll),
    _ => None,
  })
}

fn spawn_worker() -> (Sender<UiToWorker>, Receiver<WorkerToUi>, std::thread::JoinHandle<()>) {
  let handle = spawn_ui_worker("fastr-ui-worker-history-test").expect("spawn ui worker");
  handle.split()
}

fn write_fixtures(dir: &std::path::Path) -> (String, String) {
  let html = |color: &str| {
    format!(
      r#"<!doctype html>
<html>
  <head>
    <style>
      html, body {{ margin: 0; padding: 0; background: {color}; }}
      #spacer {{ height: 2000px; }}
    </style>
  </head>
  <body>
    <div id="spacer"></div>
  </body>
</html>"#
    )
  };

  std::fs::write(dir.join("a.html"), html("rgb(255, 0, 0)")).expect("write a.html");
  std::fs::write(dir.join("b.html"), html("rgb(0, 0, 255)")).expect("write b.html");

  let a_url = Url::from_file_path(dir.join("a.html"))
    .expect("file url a")
    .to_string();
  let b_url = Url::from_file_path(dir.join("b.html"))
    .expect("file url b")
    .to_string();
  (a_url, b_url)
}

#[test]
fn back_forward_toggles_can_go_flags_and_restores_page() {
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("tempdir");
  let (a_url, b_url) = write_fixtures(dir.path());

  let (tx, rx, handle) = spawn_worker();
  let tab_id = TabId(1);
  tx.send(create_tab_msg(tab_id, None))
  .unwrap();
  tx.send(viewport_changed_msg(tab_id, (64, 64), 1.0))
  .unwrap();

  tx.send(navigate_msg(tab_id, a_url.clone(), NavigationReason::TypedUrl))
  .unwrap();
  let (url, can_go_back, can_go_forward) = next_navigation_committed(&rx, tab_id);
  assert_eq!(url, a_url);
  assert!(!can_go_back);
  assert!(!can_go_forward);
  let frame_a = next_frame_ready(&rx, tab_id);
  assert_frame_has_color(&frame_a, (255, 0, 0, 255));

  tx.send(navigate_msg(tab_id, b_url.clone(), NavigationReason::TypedUrl))
  .unwrap();
  let (url, can_go_back, can_go_forward) = next_navigation_committed(&rx, tab_id);
  assert_eq!(url, b_url);
  assert!(can_go_back);
  assert!(!can_go_forward);
  let frame_b = next_frame_ready(&rx, tab_id);
  assert_frame_has_color(&frame_b, (0, 0, 255, 255));

  tx.send(UiToWorker::GoBack { tab_id }).unwrap();
  let (url, can_go_back, can_go_forward) = next_navigation_committed(&rx, tab_id);
  assert_eq!(url, a_url);
  assert!(!can_go_back);
  assert!(can_go_forward);
  let frame_back = next_frame_ready(&rx, tab_id);
  assert_frame_has_color(&frame_back, (255, 0, 0, 255));

  tx.send(UiToWorker::GoForward { tab_id }).unwrap();
  let (url, can_go_back, can_go_forward) = next_navigation_committed(&rx, tab_id);
  assert_eq!(url, b_url);
  assert!(can_go_back);
  assert!(!can_go_forward);
  let frame_forward = next_frame_ready(&rx, tab_id);
  assert_frame_has_color(&frame_forward, (0, 0, 255, 255));

  drop(tx);
  handle.join().expect("worker join");
}

#[test]
fn reload_does_not_create_new_history_entry() {
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("tempdir");
  let (a_url, b_url) = write_fixtures(dir.path());

  let (tx, rx, handle) = spawn_worker();
  let tab_id = TabId(1);
  tx.send(create_tab_msg(tab_id, None))
  .unwrap();
  tx.send(viewport_changed_msg(tab_id, (64, 64), 1.0))
  .unwrap();

  tx.send(navigate_msg(tab_id, a_url.clone(), NavigationReason::TypedUrl))
  .unwrap();
  let _ = next_navigation_committed(&rx, tab_id);
  let _ = next_frame_ready(&rx, tab_id);

  tx.send(navigate_msg(tab_id, b_url.clone(), NavigationReason::TypedUrl))
  .unwrap();
  let _ = next_navigation_committed(&rx, tab_id);
  let _ = next_frame_ready(&rx, tab_id);

  // Move back so we have forward history, then ensure reload preserves it.
  tx.send(UiToWorker::GoBack { tab_id }).unwrap();
  let (url, can_go_back, can_go_forward) = next_navigation_committed(&rx, tab_id);
  assert_eq!(url, a_url);
  assert!(!can_go_back);
  assert!(can_go_forward);
  let frame = next_frame_ready(&rx, tab_id);
  assert_frame_has_color(&frame, (255, 0, 0, 255));

  tx.send(UiToWorker::Reload { tab_id }).unwrap();
  let (url, can_go_back, can_go_forward) = next_navigation_committed(&rx, tab_id);
  assert_eq!(url, a_url);
  assert!(!can_go_back);
  assert!(can_go_forward);
  let frame = next_frame_ready(&rx, tab_id);
  assert_frame_has_color(&frame, (255, 0, 0, 255));

  tx.send(UiToWorker::GoForward { tab_id }).unwrap();
  let (url, can_go_back, can_go_forward) = next_navigation_committed(&rx, tab_id);
  assert_eq!(url, b_url);
  assert!(can_go_back);
  assert!(!can_go_forward);
  let frame = next_frame_ready(&rx, tab_id);
  assert_frame_has_color(&frame, (0, 0, 255, 255));

  drop(tx);
  handle.join().expect("worker join");
}

#[test]
fn scroll_is_restored_across_back_and_forward() {
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("tempdir");
  let (a_url, b_url) = write_fixtures(dir.path());

  let (tx, rx, handle) = spawn_worker();
  let tab_id = TabId(1);
  tx.send(create_tab_msg(tab_id, None))
  .unwrap();
  tx.send(viewport_changed_msg(tab_id, (64, 64), 1.0))
  .unwrap();

  tx.send(navigate_msg(tab_id, a_url.clone(), NavigationReason::TypedUrl))
  .unwrap();
  let _ = next_navigation_committed(&rx, tab_id);
  let _ = next_frame_ready(&rx, tab_id);
  // `FrameReady` is followed by `ScrollStateUpdated`; drain it so the subsequent scroll assertions
  // do not accidentally observe the initial (0,0) state from navigation.
  let _ = next_scroll_state_updated(&rx, tab_id);

  // Scroll on A and ensure it is saved in history.
  tx.send(scroll_msg(tab_id, (0.0, 240.0), None))
  .unwrap();
  let _frame_scrolled_a = next_frame_ready(&rx, tab_id);
  let scrolled_a = next_scroll_state_updated(&rx, tab_id);
  assert!(
    scrolled_a.viewport.y > 0.0,
    "expected scroll on A to increase, got {:?}",
    scrolled_a.viewport
  );
  let saved_scroll_y_a = scrolled_a.viewport.y;

  tx.send(navigate_msg(tab_id, b_url.clone(), NavigationReason::TypedUrl))
  .unwrap();
  let _ = next_navigation_committed(&rx, tab_id);
  let _ = next_frame_ready(&rx, tab_id);
  let _ = next_scroll_state_updated(&rx, tab_id);

  tx.send(scroll_msg(tab_id, (0.0, 400.0), None))
  .unwrap();
  // Scroll repaint emits `FrameReady` then `ScrollStateUpdated`.
  let _frame_scrolled_b = next_frame_ready(&rx, tab_id);
  let scrolled_b = next_scroll_state_updated(&rx, tab_id);
  assert!(
    scrolled_b.viewport.y > 0.0,
    "expected scroll on B to increase, got {:?}",
    scrolled_b.viewport
  );
  let saved_scroll_y_b = scrolled_b.viewport.y;

  tx.send(UiToWorker::GoBack { tab_id }).unwrap();
  let (_url, _can_go_back, can_go_forward) = next_navigation_committed(&rx, tab_id);
  assert!(can_go_forward, "expected can_go_forward after going back");
  let frame_back = next_frame_ready(&rx, tab_id);
  assert!(
    (frame_back.scroll_state.viewport.y - saved_scroll_y_a).abs() < 1.0,
    "expected back navigation to restore scroll_y ~= {saved_scroll_y_a} (got {:?})",
    frame_back.scroll_state.viewport
  );

  tx.send(UiToWorker::GoForward { tab_id }).unwrap();
  let (_url, can_go_back, can_go_forward) = next_navigation_committed(&rx, tab_id);
  assert!(can_go_back);
  assert!(!can_go_forward);
  let frame_forward = next_frame_ready(&rx, tab_id);
  assert!(
    (frame_forward.scroll_state.viewport.y - saved_scroll_y_b).abs() < 1.0,
    "expected forward navigation to restore scroll_y ~= {saved_scroll_y_b} (got {:?})",
    frame_forward.scroll_state.viewport
  );

  drop(tx);
  handle.join().expect("worker join");
}
