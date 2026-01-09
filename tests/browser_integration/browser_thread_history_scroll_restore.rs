#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, RenderedFrame, TabId, UiToWorker, WorkerToUi};
use std::time::{Duration, Instant};

// Worker startup + navigation + render can take a few seconds under parallel load (CI).
const TIMEOUT: Duration = Duration::from_secs(20);

fn recv_nav_committed(rx: &std::sync::mpsc::Receiver<WorkerToUi>, tab_id: TabId) -> (String, bool, bool) {
  let deadline = Instant::now() + TIMEOUT;
  while Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let msg = support::recv_for_tab(rx, tab_id, remaining.min(Duration::from_millis(200)), |_| true);
    let Some(msg) = msg else { continue };
    match msg {
      WorkerToUi::NavigationCommitted {
        url,
        can_go_back,
        can_go_forward,
        ..
      } => return (url, can_go_back, can_go_forward),
      WorkerToUi::NavigationFailed { url, error, .. } => {
        panic!("navigation failed for {url}: {error}");
      }
      _ => {}
    }
  }
  panic!("timed out waiting for NavigationCommitted for tab {tab_id:?}");
}

fn recv_frame(rx: &std::sync::mpsc::Receiver<WorkerToUi>, tab_id: TabId) -> RenderedFrame {
  let deadline = Instant::now() + TIMEOUT;
  while Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let msg = support::recv_for_tab(rx, tab_id, remaining.min(Duration::from_millis(200)), |_| true);
    let Some(msg) = msg else { continue };
    match msg {
      WorkerToUi::FrameReady { frame, .. } => return frame,
      WorkerToUi::NavigationFailed { url, error, .. } => {
        panic!("navigation failed while waiting for FrameReady ({url}): {error}");
      }
      _ => {}
    }
  }
  panic!("timed out waiting for FrameReady for tab {tab_id:?}");
}

#[test]
fn browser_thread_back_restores_scroll_saved_before_navigation_paint() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url_a = site.write(
    "a.html",
    r#"<!doctype html>
      <meta charset="utf-8" />
      <style>
        html, body { margin: 0; padding: 0; }
        body { background: rgb(255, 0, 0); }
        .spacer { height: 2000px; }
      </style>
      <body>
        <div class="spacer"></div>
      </body>
    "#,
  );
  let url_b = site.write(
    "b.html",
    r#"<!doctype html>
      <meta charset="utf-8" />
      <style>
        html, body { margin: 0; padding: 0; }
        body { background: rgb(0, 0, 255); }
        .spacer { height: 2000px; }
      </style>
      <body>
        <div class="spacer"></div>
      </body>
    "#,
  );

  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let fastrender::ui::BrowserWorkerHandle { tx, rx, join } = worker;

  let tab_id = TabId::new();
  tx.send(support::create_tab_msg(tab_id, Some(url_a.clone())))
    .expect("CreateTab");
  tx.send(UiToWorker::SetActiveTab { tab_id })
    .expect("SetActiveTab");
  tx.send(support::viewport_changed_msg(tab_id, (240, 120), 1.0))
    .expect("ViewportChanged");

  // Initial navigation to A.
  let (committed, can_go_back, can_go_forward) = recv_nav_committed(&rx, tab_id);
  assert_eq!(committed, url_a);
  assert!(!can_go_back);
  assert!(!can_go_forward);
  let _ = recv_frame(&rx, tab_id);

  // Scroll, then immediately navigate away. The scroll paint may be pre-empted by the navigation,
  // so history must persist the updated scroll position before pushing the new entry.
  let expected_scroll_y = 240.0;
  tx.send(support::scroll_msg(tab_id, (0.0, expected_scroll_y), None))
    .expect("Scroll");
  tx.send(support::navigate_msg(
    tab_id,
    url_b.clone(),
    NavigationReason::TypedUrl,
  ))
  .expect("Navigate B");

  let (committed, can_go_back, can_go_forward) = recv_nav_committed(&rx, tab_id);
  assert_eq!(committed, url_b);
  assert!(can_go_back);
  assert!(!can_go_forward);
  let _ = recv_frame(&rx, tab_id);

  // Going back should restore the scroll offset we had on A.
  tx.send(UiToWorker::GoBack { tab_id }).expect("GoBack");
  let (committed, can_go_back, can_go_forward) = recv_nav_committed(&rx, tab_id);
  assert_eq!(committed, url_a);
  assert!(!can_go_back);
  assert!(can_go_forward);
  let frame_back = recv_frame(&rx, tab_id);
  assert!(
    (frame_back.scroll_state.viewport.y - expected_scroll_y).abs() < 2.0,
    "expected back navigation to restore scroll_y ~= {expected_scroll_y} (got {:?})",
    frame_back.scroll_state.viewport
  );

  drop(tx);
  drop(rx);
  join.join().expect("worker join");
}

