#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::render_control::StageHeartbeat;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

// Some of these tests intentionally render "heavy" pages and may run under significant CPU
// contention when `cargo test` runs integration tests in parallel. Use a deadline + short
// `recv_timeout` slices so a long silence doesn't prematurely end a wait loop.
const WAIT_SLICE: Duration = Duration::from_millis(25);
const LONG_WAIT: Duration = Duration::from_secs(60);

#[test]
fn cancellation_on_new_navigation() {
  let _stage_lock = super::stage_listener_test_lock();
  // Slow down deadline checks so the first navigation stays in-flight long enough for the UI to
  // bump cancellation.
  let delay_guard = support::TestRenderDelayGuard::set(Some(2));

  let cancel = CancelGens::new();
  let tab_id = TabId::new();

  let worker = spawn_ui_worker("fastr-browser-worker-cancel-nav-test").expect("spawn worker");
  let fastrender::ui::UiThreadWorkerHandle { ui_tx, ui_rx, join } = worker;

  ui_tx
    .send(support::create_tab_msg_with_cancel(tab_id, None, cancel.clone()))
    .unwrap();

  cancel.bump_paint();
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 120), 1.0))
    .unwrap();

  let url1 = "about:test-heavy".to_string();
  let url2 = "about:blank".to_string();

  cancel.bump_nav();
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url1.clone(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  // Ensure the worker picked up the first navigation before we bump cancellation.
  loop {
    match ui_rx.recv_timeout(Duration::from_secs(10)) {
      Ok(WorkerToUi::NavigationStarted { tab_id: msg_id, url }) if msg_id == tab_id => {
        if url == url1 {
          break;
        }
      }
      Ok(_) => continue,
      Err(err) => panic!("timed out waiting for NavigationStarted: {err}"),
    }
  }

  // Bump nav generation while the first navigation is still in-flight; enqueue a second
  // navigation.
  cancel.bump_nav();
  drop(delay_guard);
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url2.clone(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  let mut saw_commit_url2 = false;
  let mut saw_frame_after_commit = false;

  let deadline = std::time::Instant::now() + LONG_WAIT;
  while std::time::Instant::now() < deadline {
    match ui_rx.recv_timeout(WAIT_SLICE) {
      Ok(msg) => match msg {
        WorkerToUi::NavigationCommitted { tab_id: msg_id, url, .. } if msg_id == tab_id => {
          assert_ne!(url, url1, "first navigation should not commit after cancellation");
          if url == url2 {
            saw_commit_url2 = true;
          }
        }
        WorkerToUi::FrameReady { tab_id: msg_id, .. } if msg_id == tab_id => {
          if saw_commit_url2 {
            saw_frame_after_commit = true;
            break;
          }
        }
        WorkerToUi::NavigationFailed { tab_id: msg_id, url, .. } if msg_id == tab_id => {
          // Cancellation may surface as a timeout/cancel failure; just ensure it doesn't commit.
          assert_ne!(url, url2, "second navigation should not fail");
        }
        _ => {}
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  assert!(
    saw_commit_url2 && saw_frame_after_commit,
    "expected second navigation to commit and produce a frame"
  );

  drop(ui_tx);
  join.join().expect("join worker");
}

#[test]
fn cancellation_on_scroll_drops_stale_frames() {
  let _stage_lock = super::stage_listener_test_lock();

  let cancel = CancelGens::new();
  let tab_id = TabId::new();

  let worker = spawn_ui_worker("fastr-browser-worker-cancel-scroll-test").expect("spawn worker");
  let fastrender::ui::UiThreadWorkerHandle { ui_tx, ui_rx, join } = worker;

  ui_tx
    .send(support::create_tab_msg_with_cancel(tab_id, None, cancel.clone()))
    .unwrap();

  cancel.bump_paint();
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (160, 120), 1.0))
    .unwrap();

  let url = "about:test-heavy".to_string();

  cancel.bump_nav();
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url.clone(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  // Wait for the initial navigation to commit and produce a frame.
  let mut committed = false;
  let mut saw_initial_frame = false;
  let deadline = std::time::Instant::now() + LONG_WAIT;
  while std::time::Instant::now() < deadline {
    match ui_rx.recv_timeout(WAIT_SLICE) {
      Ok(msg) => match msg {
        WorkerToUi::NavigationCommitted { tab_id: msg_id, url: committed_url, .. }
          if msg_id == tab_id =>
        {
          assert_eq!(committed_url, url);
          committed = true;
          if saw_initial_frame {
            break;
          }
        }
        WorkerToUi::FrameReady { tab_id: msg_id, .. } if msg_id == tab_id => {
          saw_initial_frame = true;
          if committed {
            break;
          }
        }
        _ => {}
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }
  assert!(
    committed && saw_initial_frame,
    "expected initial navigation to commit and produce a frame"
  );

  // Trigger a scroll repaint, then cancel it mid-flight by bumping paint generation and sending a
  // second scroll. The worker should drop any stale paint output for the first scroll.
  // Drain any straggler stage heartbeats from the navigation so the paint-stage marker below is
  // tied to the first scroll repaint.
  while ui_rx.recv_timeout(Duration::from_millis(50)).is_ok() {}

  // Slow down deadline checks so the first scroll repaint remains in-flight long enough for the
  // UI to bump paint cancellation.
  let delay_guard = support::TestRenderDelayGuard::set(Some(2));
  cancel.bump_paint();
  ui_tx
    .send(support::scroll_msg(tab_id, (0.0, 200.0), None))
    .unwrap();

  // Wait for paint to begin (paint-stage heartbeat) before triggering cancellation so we cancel
  // an in-flight job rather than a queued scroll.
  let mut pre_cancel: Vec<WorkerToUi> = Vec::new();
  let mut saw_paint_stage = false;
  while !saw_paint_stage {
    match ui_rx.recv_timeout(Duration::from_secs(10)) {
      Ok(msg) => {
        if matches!(
          &msg,
          WorkerToUi::Stage {
            tab_id: msg_id,
            stage: StageHeartbeat::PaintBuild | StageHeartbeat::PaintRasterize
          } if *msg_id == tab_id
        ) {
          saw_paint_stage = true;
        }
        pre_cancel.push(msg);
      }
      Err(err) => panic!("timed out waiting for paint stage heartbeat: {err}"),
    }
  }

  cancel.bump_paint();
  ui_tx
    .send(support::scroll_msg(tab_id, (0.0, 200.0), None))
    .unwrap();
  // The second scroll can render at full speed; we only needed the artificial delay to keep the
  // first paint busy long enough to trigger cancellation.
  drop(delay_guard);

  let mut saw_scroll1_frame = false;
  let mut saw_scroll2_frame = false;

  let mut messages = pre_cancel;
  for msg in messages.iter() {
    if let WorkerToUi::FrameReady { tab_id: msg_id, frame } = msg {
      if *msg_id != tab_id {
        continue;
      }
      let y = frame.scroll_state.viewport.y;
      if (y - 200.0).abs() < 5.0 {
        saw_scroll1_frame = true;
      }
      if (y - 400.0).abs() < 5.0 {
        saw_scroll2_frame = true;
      }
    }
  }

  while let Ok(msg) = ui_rx.recv_timeout(Duration::from_secs(30)) {
    if let WorkerToUi::FrameReady { tab_id: msg_id, frame } = &msg {
      if *msg_id == tab_id {
        let y = frame.scroll_state.viewport.y;
        if (y - 200.0).abs() < 5.0 {
          saw_scroll1_frame = true;
        }
        if (y - 400.0).abs() < 5.0 {
          saw_scroll2_frame = true;
          messages.push(msg);
          break;
        }
      }
    }
    messages.push(msg);
  }

  assert!(
    saw_scroll2_frame,
    "expected a committed frame for the second scroll repaint; got:\n{}",
    support::format_messages(&messages)
  );
  assert!(
    !saw_scroll1_frame,
    "stale frame from first scroll repaint should be dropped; got:\n{}",
    support::format_messages(&messages)
  );

  drop(ui_tx);
  join.join().expect("join worker");
}
