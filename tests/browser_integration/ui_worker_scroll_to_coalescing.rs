#![cfg(feature = "browser_ui")]

use super::support::{
  create_tab_msg, drain_for, request_repaint, scroll_to_msg, viewport_changed_msg, DEFAULT_TIMEOUT,
};
use fastrender::render_control::StageHeartbeat;
use fastrender::ui::about_pages;
use fastrender::ui::messages::{RepaintReason, TabId, WorkerToUi};
use fastrender::ui::{spawn_ui_worker, spawn_ui_worker_for_test};
use std::time::{Duration, Instant};

#[test]
fn scroll_to_burst_applies_last_position() {
  let _lock = super::stage_listener_test_lock();

  let handle = spawn_ui_worker("fastr-ui-worker-scrollto-burst").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(create_tab_msg(
      tab_id,
      Some(about_pages::ABOUT_TEST_SCROLL.to_string()),
    ))
    .expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("ViewportChanged");

  // Wait for the initial frame so the document is live.
  super::support::recv_for_tab(&ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("initial FrameReady");
  // The worker does not guarantee a `ScrollStateUpdated` after navigation; drain follow-up messages
  // instead of waiting for one so subsequent assertions are deterministic.
  let _ = drain_for(&ui_rx, Duration::from_millis(200));

  // Fire a burst of ScrollTo messages and ensure the last position wins.
  let last_y = 1234.0_f32;
  for i in 0..200 {
    let y = (i as f32) * 5.0;
    ui_tx
      .send(scroll_to_msg(tab_id, (0.0, y)))
      .expect("ScrollTo burst");
  }
  ui_tx
    .send(scroll_to_msg(tab_id, (0.0, last_y)))
    .expect("ScrollTo last");

  let msg = super::support::recv_for_tab(&ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| match msg {
    WorkerToUi::FrameReady { frame, .. } => (frame.scroll_state.viewport.y - last_y).abs() < 1.0,
    _ => false,
  })
  .expect("FrameReady after ScrollTo burst");
  let WorkerToUi::FrameReady { frame, .. } = msg else {
    unreachable!();
  };

  assert!(
    (frame.scroll_state.viewport.y - last_y).abs() < 1.0,
    "expected ScrollTo burst to settle at y~{last_y}, got {:?}",
    frame.scroll_state.viewport
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn scroll_to_burst_produces_single_frame_under_slow_render() {
  let _lock = super::stage_listener_test_lock();

  // Slow down rendering so a ScrollTo burst has time to queue up while a paint job is in flight.
  let handle = spawn_ui_worker_for_test("fastr-ui-worker-scrollto-burst-slow", Some(30))
    .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(create_tab_msg(
      tab_id,
      Some(about_pages::ABOUT_TEST_SCROLL.to_string()),
    ))
    .expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("ViewportChanged");

  // Wait for the initial frame so the document is live.
  super::support::recv_for_tab(&ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("initial FrameReady");
  // The worker does not guarantee a `ScrollStateUpdated` after navigation; drain follow-up messages
  // instead of waiting for one.
  let _ = drain_for(&ui_rx, Duration::from_millis(200));

  // Trigger a paint job and wait until it begins so our ScrollTo burst is guaranteed to arrive
  // while the worker is busy rendering.
  ui_tx
    .send(request_repaint(tab_id, RepaintReason::Explicit))
    .expect("RequestRepaint");

  super::support::recv_for_tab(&ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::Stage {
        stage: StageHeartbeat::PaintBuild | StageHeartbeat::PaintRasterize,
        ..
      }
    )
  })
  .expect("paint stage heartbeat");

  // Send a burst of ScrollTo messages while the paint job is in progress. We expect only one new
  // frame (at the final scroll position) to be emitted.
  let last_y = 1500.0_f32;
  for i in 0..400 {
    let y = (i as f32) * 4.0;
    ui_tx
      .send(scroll_to_msg(tab_id, (0.0, y)))
      .expect("ScrollTo burst");
  }
  ui_tx
    .send(scroll_to_msg(tab_id, (0.0, last_y)))
    .expect("ScrollTo last");

  // Collect the first frame after the burst.
  let deadline = Instant::now() + DEFAULT_TIMEOUT;
  let mut frames: Vec<fastrender::scroll::ScrollState> = Vec::new();
  while Instant::now() < deadline && frames.is_empty() {
    match ui_rx.recv_timeout(Duration::from_millis(50)) {
      Ok(msg) => {
        if let WorkerToUi::FrameReady { tab_id: got, frame } = msg {
          if got == tab_id {
            frames.push(frame.scroll_state);
          }
        }
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  assert!(
    !frames.is_empty(),
    "timed out waiting for FrameReady after ScrollTo burst"
  );

  // Drain briefly to detect any extra frames that would indicate the burst was not coalesced.
  for msg in drain_for(&ui_rx, Duration::from_secs(1)) {
    if let WorkerToUi::FrameReady { tab_id: got, frame } = msg {
      if got == tab_id {
        frames.push(frame.scroll_state);
      }
    }
  }

  assert_eq!(
    frames.len(),
    1,
    "expected exactly one FrameReady after ScrollTo burst; frames={frames:?}"
  );
  assert!(
    (frames[0].viewport.y - last_y).abs() < 1.0,
    "expected final frame to use y~{last_y}, got {:?}",
    frames[0].viewport
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
