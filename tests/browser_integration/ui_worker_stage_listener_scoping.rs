#![cfg(feature = "browser_ui")]

use fastrender::render_control::{record_stage, StageHeartbeat};
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::worker_loop::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use tempfile::tempdir;

use super::support::{create_tab_msg, navigate_msg, viewport_changed_msg};

fn wait_for_navigation_complete(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> bool {
  let deadline = Instant::now() + Duration::from_secs(10);
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
fn stage_listener_is_cleared_after_navigation_job() {
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
      <body>hello</body>
    </html>
  "#;
  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = format!("file://{}/index.html", dir.path().display());

  let handle =
    spawn_ui_worker("fastr-ui-worker-stage-listener-scope").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);
  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("CreateTab");
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

  // The stage listener is global across the process; once the job completes it must be cleared.
  record_stage(StageHeartbeat::DomParse);
  assert!(
    ui_rx.recv_timeout(Duration::from_millis(200)).is_err(),
    "expected no WorkerToUi messages after record_stage()"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
