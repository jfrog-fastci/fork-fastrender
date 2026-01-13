#![cfg(feature = "browser_ui")]

use super::support::{
  create_tab_msg, navigate_msg, scroll_msg, viewport_changed_msg, DEFAULT_TIMEOUT,
};
use fastrender::render_control::StageHeartbeat;
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn wait_for_navigation_complete(rx: &fastrender::ui::WorkerToUiInbox, tab_id: TabId) -> bool {
  let deadline = Instant::now() + DEFAULT_TIMEOUT;
  let mut saw_frame = false;
  let mut saw_loading_done = false;
  let mut saw_stage = false;

  while !(saw_frame && saw_loading_done) {
    let now = Instant::now();
    let remaining = deadline
      .checked_duration_since(now)
      .unwrap_or(Duration::from_secs(0));
    assert!(
      remaining > Duration::from_secs(0),
      "timed out waiting for navigation completion"
    );
    let msg = rx
      .recv_timeout(remaining)
      .unwrap_or_else(|err| panic!("timed out waiting for WorkerToUi message: {err}"));
    match msg {
      WorkerToUi::Stage { tab_id: got, .. } if got == tab_id => {
        saw_stage = true;
      }
      WorkerToUi::FrameReady { tab_id: got, .. } if got == tab_id => {
        saw_frame = true;
      }
      WorkerToUi::LoadingState {
        tab_id: got,
        loading: false,
      } if got == tab_id => {
        saw_loading_done = true;
      }
      _ => {}
    }
  }

  saw_stage
}

#[test]
fn stage_heartbeats_forwarded_for_scroll_repaint_after_navigation() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          body { background: rgb(1, 2, 3); }
        </style>
      </head>
      <body>
        hello
        <div style="height: 2000px;"></div>
      </body>
    </html>
  "#;
  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = url::Url::from_file_path(dir.path().join("index.html"))
    .unwrap()
    .to_string();

  let handle = spawn_ui_worker("fastr-ui-worker-stage-listener-scope").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);
  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  // Wait for navigation completion: FrameReady is sent before the navigation function returns,
  // while LoadingState(false) is emitted after the job's stage listener guard has been dropped.
  let saw_stage = wait_for_navigation_complete(&ui_rx, tab_id);
  assert!(
    saw_stage,
    "expected at least one WorkerToUi::Stage message during navigation"
  );

  // Drain any pending messages (including stage heartbeats emitted during the navigation job).
  while ui_rx.try_recv().is_ok() {}

  // Stage forwarding must be scoped to each render job. Scrolling triggers a repaint, and the
  // worker should emit stage messages for that paint without including navigation-specific fetch
  // stages (e.g. ReadCache/FollowRedirects).
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 80.0), None))
    .expect("Scroll");

  let deadline = Instant::now() + DEFAULT_TIMEOUT;
  let mut saw_scroll_frame = false;
  let mut stages_after_scroll = Vec::new();
  while Instant::now() < deadline && !saw_scroll_frame {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match ui_rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(msg) => match msg {
        WorkerToUi::Stage { tab_id: got, stage } if got == tab_id => {
          stages_after_scroll.push(stage);
        }
        WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => {
          if frame.scroll_state.viewport.y > 0.0 {
            saw_scroll_frame = true;
          }
        }
        _ => {}
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }
  assert!(saw_scroll_frame, "expected FrameReady after scroll");
  assert!(
    !stages_after_scroll.is_empty(),
    "expected stage heartbeats during scroll repaint"
  );
  assert!(
    stages_after_scroll.iter().any(|stage| matches!(
      stage,
      StageHeartbeat::PaintBuild | StageHeartbeat::PaintRasterize
    )),
    "expected paint stage heartbeats during scroll repaint: {stages_after_scroll:?}"
  );
  assert!(
    !stages_after_scroll.iter().any(|stage| matches!(
      stage,
      StageHeartbeat::ReadCache | StageHeartbeat::FollowRedirects
    )),
    "unexpected fetch stage heartbeats during scroll repaint: {stages_after_scroll:?}"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
