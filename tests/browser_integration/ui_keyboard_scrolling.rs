#![cfg(feature = "browser_ui")]

use fastrender::ui::cancel::CancelGens;
use fastrender::ui::{spawn_browser_worker, NavigationReason, TabId, WorkerToUi};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use super::support::{
  create_tab_msg_with_cancel, drain_for, format_messages, key_action, navigate_msg, rgba_at,
  scroll_msg, viewport_changed_msg, TempSite, DEFAULT_TIMEOUT,
};

fn wait_for_initial_frame(
  rx: &Receiver<WorkerToUi>,
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

fn wait_for_scroll_response(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  timeout: Duration,
  mut pred: impl FnMut(f32) -> bool,
) -> (f32, f32) {
  let deadline = Instant::now() + timeout;
  let mut frame_y: Option<f32> = None;
  let mut seen: Vec<WorkerToUi> = Vec::new();

  while Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(msg) => {
        if let WorkerToUi::FrameReady { tab_id: got, frame } = &msg {
          if *got == tab_id && pred(frame.scroll_state.viewport.y) {
            frame_y = Some(frame.scroll_state.viewport.y);
          }
        }
        if seen.len() < 64 {
          seen.push(msg);
        }
        if frame_y.is_some() {
          break;
        }
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  let Some(frame_y) = frame_y else {
    panic!(
      "timed out waiting for FrameReady satisfying predicate\nmessages:\n{}",
      format_messages(&seen)
    );
  };
  (frame_y, frame_y)
}

#[test]
fn keyboard_scroll_actions_update_viewport_scroll_state() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let fastrender::ui::BrowserWorkerHandle { tx, rx, join } =
    spawn_browser_worker().expect("spawn browser worker");

  let tab_id = TabId(1);
  let cancel = CancelGens::new();
  tx.send(create_tab_msg_with_cancel(tab_id, None, cancel))
    .unwrap();
  // Use a round height so viewport-height * 0.9 is easy to reason about (100 -> 90).
  let viewport_css = (200, 100);
  tx.send(viewport_changed_msg(tab_id, viewport_css, 1.0))
    .unwrap();
  tx.send(navigate_msg(
    tab_id,
    "about:test-scroll".to_string(),
    NavigationReason::TypedUrl,
  ))
  .unwrap();

  let initial_frame = wait_for_initial_frame(&rx, tab_id);
  let mut y = initial_frame.scroll_state.viewport.y;

  assert!(
    y.abs() < 1e-3,
    "expected initial scroll y to start at 0, got {y}"
  );

  let step_y = (viewport_css.1 as f32) * 0.9;

  // PageDown should scroll down by ~0.9 * viewport height.
  tx.send(key_action(tab_id, fastrender::interaction::KeyAction::PageDown))
    .unwrap();
  let (_scroll_y, frame_y) =
    wait_for_scroll_response(&rx, tab_id, DEFAULT_TIMEOUT, |next| next > y + 1.0);
  assert!(
    (frame_y - step_y).abs() < 1.0,
    "expected PageDown to scroll by ~{step_y} (0.9*viewport height), got {frame_y}"
  );
  y = frame_y;

  // PageUp should scroll back up by ~0.9 * viewport height (and clamp at the top).
  tx.send(key_action(tab_id, fastrender::interaction::KeyAction::PageUp))
    .unwrap();
  let (_scroll_y, frame_y) = wait_for_scroll_response(&rx, tab_id, DEFAULT_TIMEOUT, |next| {
    next < y - 1.0 || next <= 1.0
  });
  assert!(
    frame_y <= 1.0,
    "expected PageUp to scroll back toward the top, got {frame_y}"
  );
  y = frame_y;

  // End.
  tx.send(key_action(tab_id, fastrender::interaction::KeyAction::End))
    .unwrap();
  let (_scroll_y, frame_y) = wait_for_scroll_response(&rx, tab_id, DEFAULT_TIMEOUT, |next| {
    next > y + 10.0 && next > 3_000.0
  });
  assert!(
    frame_y > 3_000.0,
    "expected End to scroll near bottom, got {frame_y}"
  );
  y = frame_y;

  // Home.
  tx.send(key_action(tab_id, fastrender::interaction::KeyAction::Home))
    .unwrap();
  let (_scroll_y, frame_y) =
    wait_for_scroll_response(&rx, tab_id, DEFAULT_TIMEOUT, |next| next <= 1.0);
  assert!(
    frame_y <= 1.0,
    "expected Home to scroll to top, got {frame_y}"
  );

  drop(tx);
  join.join().unwrap();
}

#[test]
fn home_end_space_keys_scroll_when_no_element_is_focused() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let fastrender::ui::BrowserWorkerHandle { tx, rx, join } =
    spawn_browser_worker().expect("spawn browser worker");

  let tab_id = TabId(1);
  let cancel = CancelGens::new();
  tx.send(create_tab_msg_with_cancel(tab_id, None, cancel))
    .unwrap();
  // Use a round height so viewport-height * 0.9 is easy to reason about (100 -> 90).
  let viewport_css = (200, 100);
  tx.send(viewport_changed_msg(tab_id, viewport_css, 1.0))
    .unwrap();
  tx.send(navigate_msg(
    tab_id,
    "about:test-scroll".to_string(),
    NavigationReason::TypedUrl,
  ))
  .unwrap();

  let initial_frame = wait_for_initial_frame(&rx, tab_id);
  let mut y = initial_frame.scroll_state.viewport.y;

  assert!(
    y.abs() < 1e-3,
    "expected initial scroll y to start at 0, got {y}"
  );

  // `End` should scroll near the bottom when nothing is focused.
  tx.send(key_action(tab_id, fastrender::interaction::KeyAction::End))
    .unwrap();
  let (_scroll_y, frame_y) = wait_for_scroll_response(&rx, tab_id, DEFAULT_TIMEOUT, |next| {
    next > y + 10.0 && next > 3_000.0
  });
  assert!(
    frame_y > 3_000.0,
    "expected End to scroll near bottom, got {frame_y}"
  );

  // `Home` should scroll back to the top.
  tx.send(key_action(tab_id, fastrender::interaction::KeyAction::Home))
    .unwrap();
  let (_scroll_y, frame_y) =
    wait_for_scroll_response(&rx, tab_id, DEFAULT_TIMEOUT, |next| next <= 1.0);
  assert!(
    frame_y <= 1.0,
    "expected Home to scroll back to the top, got {frame_y}"
  );
  y = frame_y;

  // `Shift+End` should scroll near the bottom as well (Shift should not disable browser-style
  // Home/End scrolling when nothing is focused).
  tx.send(key_action(
    tab_id,
    fastrender::interaction::KeyAction::ShiftEnd,
  ))
  .unwrap();
  let (_scroll_y, frame_y) = wait_for_scroll_response(&rx, tab_id, DEFAULT_TIMEOUT, |next| {
    next > y + 10.0 && next > 3_000.0
  });
  assert!(
    frame_y > 3_000.0,
    "expected Shift+End to scroll near bottom, got {frame_y}"
  );

  // `Shift+Home` should scroll back to the top as well.
  tx.send(key_action(
    tab_id,
    fastrender::interaction::KeyAction::ShiftHome,
  ))
  .unwrap();
  let (_scroll_y, frame_y) =
    wait_for_scroll_response(&rx, tab_id, DEFAULT_TIMEOUT, |next| next <= 1.0);
  assert!(
    frame_y <= 1.0,
    "expected Shift+Home to scroll back to the top, got {frame_y}"
  );
  y = frame_y;

  // `Space` should scroll down by ~0.9 * viewport height.
  let step_y = (viewport_css.1 as f32) * 0.9;
  tx.send(key_action(
    tab_id,
    fastrender::interaction::KeyAction::Space,
  ))
  .unwrap();
  let (_scroll_y, frame_y) =
    wait_for_scroll_response(&rx, tab_id, DEFAULT_TIMEOUT, |next| next > y + 1.0);
  assert!(
    (frame_y - step_y).abs() < 1.0,
    "expected Space to scroll by ~{step_y}, got {frame_y}"
  );
  y = frame_y;

  // ArrowDown / ArrowUp should scroll by a small fixed step when nothing is focused.
  let arrow_step = 40.0;
  tx.send(key_action(
    tab_id,
    fastrender::interaction::KeyAction::ArrowDown,
  ))
  .unwrap();
  let (_scroll_y, frame_y) =
    wait_for_scroll_response(&rx, tab_id, DEFAULT_TIMEOUT, |next| next > y + 1.0);
  assert!(
    (frame_y - (y + arrow_step)).abs() < 1.0,
    "expected ArrowDown to scroll by ~{arrow_step}, got {frame_y} (from {y})"
  );
  y = frame_y;

  tx.send(key_action(
    tab_id,
    fastrender::interaction::KeyAction::ArrowUp,
  ))
  .unwrap();
  let (_scroll_y, frame_y) =
    wait_for_scroll_response(&rx, tab_id, DEFAULT_TIMEOUT, |next| next < y - 1.0);
  assert!(
    (frame_y - step_y).abs() < 1.0,
    "expected ArrowUp to scroll back to ~{step_y}, got {frame_y}"
  );
  y = frame_y;

  // `Shift+Space` should scroll back up by ~0.9 * viewport height.
  tx.send(key_action(
    tab_id,
    fastrender::interaction::KeyAction::ShiftSpace,
  ))
  .unwrap();
  let (_scroll_y, frame_y) = wait_for_scroll_response(&rx, tab_id, DEFAULT_TIMEOUT, |next| {
    next < y - 1.0 || next <= 1.0
  });
  assert!(
    frame_y <= 1.0,
    "expected Shift+Space to scroll back to the top, got {frame_y}"
  );

  drop(tx);
  join.join().unwrap();
}

#[test]
fn space_scrolls_when_link_is_focused() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let fastrender::ui::BrowserWorkerHandle { tx, rx, join } =
    spawn_browser_worker().expect("spawn browser worker");

  let tab_id = TabId(1);
  let cancel = CancelGens::new();
  tx.send(create_tab_msg_with_cancel(tab_id, None, cancel))
    .unwrap();
  // Use a round height so viewport-height * 0.9 is easy to reason about (100 -> 90).
  let viewport_css = (200, 100);
  tx.send(viewport_changed_msg(tab_id, viewport_css, 1.0))
    .unwrap();
  tx.send(navigate_msg(
    tab_id,
    "about:test-scroll".to_string(),
    NavigationReason::TypedUrl,
  ))
  .unwrap();

  let initial_frame = wait_for_initial_frame(&rx, tab_id);
  let mut y = initial_frame.scroll_state.viewport.y;

  assert!(
    y.abs() < 1e-3,
    "expected initial scroll y to start at 0, got {y}"
  );

  // Focus the top-of-page link using Tab. (The `about:test-scroll` page includes an anchor at the
  // top specifically for this interaction test.)
  tx.send(key_action(tab_id, fastrender::interaction::KeyAction::Tab))
    .unwrap();

  // Space should still scroll the viewport (even though the link is focused).
  let step_y = (viewport_css.1 as f32) * 0.9;
  tx.send(key_action(
    tab_id,
    fastrender::interaction::KeyAction::Space,
  ))
  .unwrap();
  let (_scroll_y, frame_y) =
    wait_for_scroll_response(&rx, tab_id, DEFAULT_TIMEOUT, |next| next > y + 1.0);
  assert!(
    (frame_y - step_y).abs() < 1.0,
    "expected Space to scroll by ~{step_y} when a link is focused, got {frame_y}"
  );
  y = frame_y;

  // Shift+Space should scroll back up.
  tx.send(key_action(
    tab_id,
    fastrender::interaction::KeyAction::ShiftSpace,
  ))
  .unwrap();
  let (_scroll_y, frame_y) =
    wait_for_scroll_response(&rx, tab_id, DEFAULT_TIMEOUT, |next| next < y - 1.0 || next <= 1.0);
  assert!(
    frame_y <= 1.0,
    "expected Shift+Space to scroll back to the top when a link is focused, got {frame_y}"
  );

  drop(tx);
  join.join().unwrap();
}

#[test]
fn video_controls_consume_space_and_arrow_keys() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let fastrender::ui::BrowserWorkerHandle { tx, rx, join } =
    spawn_browser_worker().expect("spawn browser worker");

  let tab_id = TabId(1);
  let cancel = CancelGens::new();
  tx.send(create_tab_msg_with_cancel(tab_id, None, cancel))
    .unwrap();
  let viewport_css = (200, 100);
  tx.send(viewport_changed_msg(tab_id, viewport_css, 1.0))
    .unwrap();

  let site = TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body { margin: 0; padding: 0; }
            #v {
              display: block;
              width: 200px;
              height: 40px;
              margin: 0;
              padding: 0;
              border: 0;
              background: rgb(0, 0, 0);
            }
            #marker { width: 200px; height: 20px; background: rgb(255, 0, 0); }
            video:focus + #marker { background: rgb(0, 255, 0); }
            /* Make the page horizontally scrollable as well so ArrowLeft/ArrowRight fallback
               scrolling would be observable if it were incorrectly triggered. */
            #spacer { height: 5000px; width: 2000px; }
          </style>
        </head>
        <body>
          <video id="v" controls tabindex="0"></video>
          <div id="marker"></div>
          <div id="spacer"></div>
        </body>
      </html>"#,
  );

  tx.send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .unwrap();

  let frame0 = wait_for_initial_frame(&rx, tab_id);
  assert!(
    frame0.scroll_metrics.bounds_css.max_y > 100.0,
    "expected test page to be scrollable, got scroll metrics {:?}",
    frame0.scroll_metrics
  );
  assert_eq!(
    rgba_at(&frame0.pixmap, 10, 50),
    [255, 0, 0, 255],
    "expected marker to start red before the video is focused"
  );

  // Focus the video element with Tab.
  tx.send(key_action(tab_id, fastrender::interaction::KeyAction::Tab))
    .unwrap();
  let frame1 = wait_for_initial_frame(&rx, tab_id);
  assert_eq!(
    rgba_at(&frame1.pixmap, 10, 50),
    [0, 255, 0, 255],
    "expected marker to turn green once the <video> is focused"
  );
  let baseline_viewport = frame1.scroll_state.viewport;

  // Ensure any follow-up paint/scroll messages from focusing are drained before the scroll-key
  // assertions below.
  let _ = drain_for(&rx, Duration::from_millis(200));

  // Scroll the viewport horizontally so ArrowLeft can be tested as well (ArrowLeft at x=0 would
  // clamp and appear as a no-op even if fallback scrolling were incorrectly triggered).
  tx.send(scroll_msg(tab_id, (80.0, 0.0), None)).unwrap();
  let baseline_viewport = super::support::recv_for_tab(&rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::FrameReady { frame, .. } if frame.scroll_state.viewport.x > baseline_viewport.x + 1.0
    )
  })
  .and_then(|msg| match msg {
    WorkerToUi::FrameReady { frame, .. } => Some(frame.scroll_state.viewport),
    _ => None,
  })
  .expect("timed out waiting for horizontal scroll baseline");

  // Drain any messages produced by the baseline scroll.
  let _ = drain_for(&rx, Duration::from_millis(200));

  let assert_no_viewport_scroll_change = |msgs: &[WorkerToUi]| {
    for msg in msgs {
      match msg {
        WorkerToUi::ScrollStateUpdated { tab_id: got, scroll } if *got == tab_id => {
          assert!(
            (scroll.viewport.x - baseline_viewport.x).abs() < 1e-3
              && (scroll.viewport.y - baseline_viewport.y).abs() < 1e-3,
            "expected viewport scroll state to remain unchanged (baseline {:?}), got {:?}\nmessages:\n{}",
            baseline_viewport,
            scroll.viewport,
            format_messages(msgs)
          );
        }
        WorkerToUi::FrameReady { tab_id: got, frame } if *got == tab_id => {
          assert!(
            (frame.scroll_state.viewport.x - baseline_viewport.x).abs() < 1e-3
              && (frame.scroll_state.viewport.y - baseline_viewport.y).abs() < 1e-3,
            "expected FrameReady viewport scroll state to remain unchanged (baseline {:?}), got {:?}\nmessages:\n{}",
            baseline_viewport,
            frame.scroll_state.viewport,
            format_messages(msgs)
          );
        }
        _ => {}
      }
    }
  };

  // Space should not trigger worker fallback page scrolling when video controls are focused.
  tx.send(key_action(
    tab_id,
    fastrender::interaction::KeyAction::Space,
  ))
  .unwrap();
  let msgs = drain_for(&rx, Duration::from_secs(1));
  assert_no_viewport_scroll_change(&msgs);

  // ArrowDown should not trigger worker fallback page scrolling when video controls are focused.
  tx.send(key_action(
    tab_id,
    fastrender::interaction::KeyAction::ArrowDown,
  ))
  .unwrap();
  let msgs = drain_for(&rx, Duration::from_secs(1));
  assert_no_viewport_scroll_change(&msgs);

  // ArrowRight/ArrowLeft should also be consumed by video controls (and must not trigger horizontal
  // fallback scrolling).
  tx.send(key_action(
    tab_id,
    fastrender::interaction::KeyAction::ArrowRight,
  ))
  .unwrap();
  let msgs = drain_for(&rx, Duration::from_secs(1));
  assert_no_viewport_scroll_change(&msgs);

  tx.send(key_action(
    tab_id,
    fastrender::interaction::KeyAction::ArrowLeft,
  ))
  .unwrap();
  let msgs = drain_for(&rx, Duration::from_secs(1));
  assert_no_viewport_scroll_change(&msgs);

  drop(tx);
  join.join().unwrap();
}
