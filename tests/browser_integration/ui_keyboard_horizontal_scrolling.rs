#![cfg(feature = "browser_ui")]

use fastrender::ui::cancel::CancelGens;
use fastrender::ui::{spawn_browser_worker, NavigationReason, TabId, WorkerToUi};
use std::time::{Duration, Instant};

use super::support::{
  create_tab_msg_with_cancel, format_messages, key_action, navigate_msg, viewport_changed_msg,
  DEFAULT_TIMEOUT, TempSite,
};

fn wait_for_initial_frame(
  rx: &impl super::support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
) -> fastrender::ui::RenderedFrame {
  super::support::recv_for_tab(rx, tab_id, DEFAULT_TIMEOUT * 2, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .and_then(|msg| match msg {
    WorkerToUi::FrameReady { frame, .. } => Some(frame),
    _ => None,
  })
  .expect("timed out waiting for initial FrameReady")
}

fn wait_for_scroll_response_x(
  rx: &impl super::support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
  timeout: Duration,
  mut pred: impl FnMut(f32) -> bool,
) -> f32 {
  let deadline = Instant::now() + timeout;
  let mut frame_x: Option<f32> = None;
  let mut seen: Vec<WorkerToUi> = Vec::new();

  while Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(msg) => {
        if let WorkerToUi::FrameReady { tab_id: got, frame } = &msg {
          if *got == tab_id && pred(frame.scroll_state.viewport.x) {
            frame_x = Some(frame.scroll_state.viewport.x);
          }
        }
        if seen.len() < 64 {
          seen.push(msg);
        }
        if frame_x.is_some() {
          break;
        }
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  let Some(frame_x) = frame_x else {
    panic!(
      "timed out waiting for FrameReady satisfying predicate\nmessages:\n{}",
      format_messages(&seen)
    );
  };
  frame_x
}

#[test]
fn arrow_left_right_scroll_horizontally_when_nothing_is_focused() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = TempSite::new();
  let page_url = site.write(
    "wide.html",
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body { margin: 0; padding: 0; }
            body { width: 2000px; height: 100px; background: rgb(255, 255, 255); }
            #marker { width: 2000px; height: 10px; background: rgb(255, 0, 0); }
          </style>
        </head>
        <body>
          <div id="marker"></div>
        </body>
      </html>
    "#,
  );

  let fastrender::ui::BrowserWorkerHandle { tx, rx, join } =
    spawn_browser_worker().expect("spawn browser worker");

  let tab_id = TabId(1);
  let cancel = CancelGens::new();
  tx.send(create_tab_msg_with_cancel(tab_id, None, cancel))
    .unwrap();
  tx.send(viewport_changed_msg(tab_id, (200, 120), 1.0))
    .unwrap();
  tx.send(navigate_msg(tab_id, page_url, NavigationReason::TypedUrl))
    .unwrap();

  let initial_frame = wait_for_initial_frame(&rx, tab_id);
  let mut x = initial_frame.scroll_state.viewport.x;

  assert!(
    x.abs() < 1e-3,
    "expected initial scroll x to start at 0, got {x}"
  );

  // ArrowRight should scroll horizontally by a small fixed step when nothing is focused.
  let arrow_step = 40.0;
  tx.send(key_action(
    tab_id,
    fastrender::interaction::KeyAction::ArrowRight,
  ))
  .unwrap();
  let frame_x = wait_for_scroll_response_x(&rx, tab_id, DEFAULT_TIMEOUT, |next| next > x + 1.0);
  assert!(
    (frame_x - (x + arrow_step)).abs() < 1.0,
    "expected ArrowRight to scroll by ~{arrow_step}, got {frame_x} (from {x})"
  );
  x = frame_x;

  // ArrowLeft should scroll back toward the left edge, clamping at 0.
  tx.send(key_action(
    tab_id,
    fastrender::interaction::KeyAction::ArrowLeft,
  ))
  .unwrap();
  let frame_x = wait_for_scroll_response_x(&rx, tab_id, DEFAULT_TIMEOUT, |next| {
    next < x - 1.0 || next <= 1.0
  });
  assert!(
    frame_x <= 1.0,
    "expected ArrowLeft to scroll back toward 0, got {frame_x}"
  );
  x = frame_x;

  // Shift+ArrowRight should behave like ArrowRight for scrolling.
  tx.send(key_action(
    tab_id,
    fastrender::interaction::KeyAction::ShiftArrowRight,
  ))
  .unwrap();
  let frame_x = wait_for_scroll_response_x(&rx, tab_id, DEFAULT_TIMEOUT, |next| next > x + 1.0);
  assert!(
    (frame_x - (x + arrow_step)).abs() < 1.0,
    "expected ShiftArrowRight to scroll by ~{arrow_step}, got {frame_x} (from {x})"
  );
  x = frame_x;

  // Shift+ArrowLeft should scroll back toward the left edge, clamping at 0.
  tx.send(key_action(
    tab_id,
    fastrender::interaction::KeyAction::ShiftArrowLeft,
  ))
  .unwrap();
  let frame_x = wait_for_scroll_response_x(&rx, tab_id, DEFAULT_TIMEOUT, |next| {
    next < x - 1.0 || next <= 1.0
  });
  assert!(
    frame_x <= 1.0,
    "expected ShiftArrowLeft to scroll back toward 0, got {frame_x}"
  );

  drop(tx);
  join.join().unwrap();
}
