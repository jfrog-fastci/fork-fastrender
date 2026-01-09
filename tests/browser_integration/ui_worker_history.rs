#![cfg(feature = "browser_ui")]

use fastrender::scroll::ScrollState;
use fastrender::ui::messages::WorkerToUi;
use fastrender::ui::{NavigationReason, RenderedFrame, TabId, UiToWorker, UiWorker};
use fastrender::FastRender;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};
use tempfile::tempdir;
use url::Url;

use super::support::{create_tab_msg, navigate_msg, scroll_msg, viewport_changed_msg};

// These tests run alongside other render-heavy integration tests; allow extra slack to avoid
// flakes under CPU contention.
const TIMEOUT: Duration = Duration::from_secs(15);

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let idx = (y * pixmap.width() + x) as usize * 4;
  (
    pixmap.data()[idx],
    pixmap.data()[idx + 1],
    pixmap.data()[idx + 2],
    pixmap.data()[idx + 3],
  )
}

fn assert_frame_has_color(frame: &RenderedFrame, expected: (u8, u8, u8, u8)) {
  let pixmap = &frame.pixmap;
  assert!(pixmap.width() > 0 && pixmap.height() > 0, "expected non-empty pixmap");
  // Sample pixels away from the right/bottom edges to avoid flaking when scrollbars are rendered.
  let x1 = if pixmap.width() > 1 { 1 } else { 0 };
  let y1 = if pixmap.height() > 1 { 1 } else { 0 };
  let samples = [
    pixel(pixmap, x1, y1),
    pixel(pixmap, pixmap.width() / 2, pixmap.height() / 2),
    pixel(pixmap, x1, pixmap.height() / 2),
    pixel(pixmap, pixmap.width() / 2, y1),
  ];
  for (idx, sample) in samples.into_iter().enumerate() {
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
  let renderer = FastRender::new().expect("renderer");
  let (to_ui_tx, to_ui_rx) = std::sync::mpsc::channel();
  let (to_worker_tx, to_worker_rx) = std::sync::mpsc::channel();
  let worker = UiWorker::new(renderer, to_ui_tx);
  let handle = std::thread::spawn(move || worker.run(to_worker_rx));
  (to_worker_tx, to_ui_rx, handle)
}

fn write_fixtures(dir: &std::path::Path) -> (String, String) {
  let html = |color: &str| {
    format!(
      r#"<!doctype html>
<html>
  <head>
    <style>
      html, body {{ margin: 0; padding: 0; }}
      body {{ background: {color}; }}
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

  tx.send(navigate_msg(tab_id, a_url.clone(), NavigationReason::BackForward))
  .unwrap();
  let (url, can_go_back, can_go_forward) = next_navigation_committed(&rx, tab_id);
  assert_eq!(url, a_url);
  assert!(!can_go_back);
  assert!(can_go_forward);
  let frame_back = next_frame_ready(&rx, tab_id);
  assert_frame_has_color(&frame_back, (255, 0, 0, 255));

  tx.send(navigate_msg(tab_id, b_url.clone(), NavigationReason::BackForward))
  .unwrap();
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

  tx.send(navigate_msg(tab_id, b_url.clone(), NavigationReason::Reload))
  .unwrap();
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
fn scroll_is_restored_when_going_back() {
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

  tx.send(scroll_msg(tab_id, (0.0, 400.0), None))
  .unwrap();
  let scrolled = next_scroll_state_updated(&rx, tab_id);
  assert!(
    scrolled.viewport.y > 0.0,
    "expected scroll to increase, got {:?}",
    scrolled.viewport
  );
  let saved_scroll_y = scrolled.viewport.y;
  let frame_scrolled = next_frame_ready(&rx, tab_id);
  assert!(
    (frame_scrolled.scroll_state.viewport.y - saved_scroll_y).abs() < 1.0,
    "frame scroll should match ScrollStateUpdated (frame={:?}, saved={saved_scroll_y})",
    frame_scrolled.scroll_state.viewport
  );

  tx.send(navigate_msg(tab_id, b_url.clone(), NavigationReason::TypedUrl))
  .unwrap();
  let _ = next_navigation_committed(&rx, tab_id);
  let _ = next_frame_ready(&rx, tab_id);

  tx.send(navigate_msg(tab_id, a_url.clone(), NavigationReason::BackForward))
  .unwrap();
  let _ = next_navigation_committed(&rx, tab_id);
  let frame = next_frame_ready(&rx, tab_id);
  assert!(
    (frame.scroll_state.viewport.y - saved_scroll_y).abs() < 1.0,
    "expected scroll restoration when going back (got {:?}, expected {saved_scroll_y})",
    frame.scroll_state.viewport
  );

  drop(tx);
  handle.join().expect("worker join");
}
