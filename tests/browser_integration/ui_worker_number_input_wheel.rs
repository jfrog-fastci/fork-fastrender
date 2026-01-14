#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, PointerButton, RenderedFrame, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(20);

fn rgba_at_css(frame: &RenderedFrame, x_css: u32, y_css: u32) -> [u8; 4] {
  let x_px = ((x_css as f32) * frame.dpr).round() as u32;
  let y_px = ((y_css as f32) * frame.dpr).round() as u32;
  support::rgba_at(&frame.pixmap, x_px, y_px)
}

fn recv_until_frame_ready(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
  deadline: Instant,
  context: &'static str,
) -> RenderedFrame {
  let mut seen: Vec<WorkerToUi> = Vec::new();
  loop {
    let now = Instant::now();
    if now >= deadline {
      panic!(
        "timed out waiting for FrameReady ({context}); saw:\n{}",
        support::format_messages(&seen)
      );
    }
    let remaining = deadline.saturating_duration_since(now);
    match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(msg) => match msg {
        WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => return frame,
        other => seen.push(other),
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
        panic!(
          "worker channel disconnected while waiting for FrameReady; saw:\n{}",
          support::format_messages(&seen)
        );
      }
    };
  }
}

fn recv_until_pixel(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
  css_pos: (u32, u32),
  expected: [u8; 4],
  deadline: Instant,
) -> RenderedFrame {
  loop {
    let frame = recv_until_frame_ready(rx, tab_id, deadline, "waiting for pixel");
    let rgba = rgba_at_css(&frame, css_pos.0, css_pos.1);
    if rgba == expected {
      return frame;
    }
  }
}

fn recv_scroll_state_updated(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
  deadline: Instant,
  context: &'static str,
) -> fastrender::scroll::ScrollState {
  loop {
    let now = Instant::now();
    if now >= deadline {
      let msgs = support::drain_for(rx, Duration::from_millis(200));
      panic!(
        "timed out waiting for ScrollStateUpdated ({context}); saw:\n{}",
        support::format_messages(&msgs)
      );
    }
    let remaining = deadline.saturating_duration_since(now);
    if let Some(msg) = support::recv_for_tab(rx, tab_id, remaining.min(Duration::from_millis(200)), |msg| {
      matches!(msg, WorkerToUi::ScrollStateUpdated { .. })
    }) {
      if let WorkerToUi::ScrollStateUpdated { scroll, .. } = msg {
        return scroll;
      }
    }
  }
}

#[test]
fn number_input_wheel_over_focused_input_steps_value_instead_of_scrolling() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page_url = site.write(
    "page.html",
    r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      /* Use in-flow spacers to guarantee scrollable height (absolute positioned content may not
         contribute to scroll bounds in the engine). */
      #pad { height: 600px; }
      #tail { height: 2000px; }

      #n { display: block; width: 120px; height: 32px; border: 0; padding: 0; margin: 0; }
      #box { width: 64px; height: 64px; margin-top: 32px; background: rgb(255, 0, 0); }
      input[value="1"] ~ #box { background: rgb(0, 255, 0); }
    </style>
  </head>
  <body>
    <div id="pad"></div>
    <input id="n" type="number" value="0">
    <div id="box"></div>
    <div id="tail"></div>
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-number-input-wheel").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (240, 180), 1.0))
    .expect("viewport");
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      page_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate");

  let deadline = Instant::now() + TIMEOUT;
  let _ = recv_until_frame_ready(&ui_rx, tab_id, deadline, "after initial navigation");
  let _ = recv_scroll_state_updated(&ui_rx, tab_id, deadline, "after initial navigation frame");

  // Drain queued messages from initial navigation so assertions are scoped to scroll+wheel.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  // Scroll down so the input is visible while `scroll_state.viewport.y > 0`. Pick an offset that
  // keeps the focused input comfortably within the viewport so focus-driven auto-scroll does not
  // adjust `scroll_y` (we want the wheel gesture to be the only thing that could scroll).
  ui_tx
    .send(support::scroll_to_msg(tab_id, (0.0, 560.0)))
    .expect("scroll to input");

  let deadline = Instant::now() + TIMEOUT;
  let frame = recv_until_frame_ready(&ui_rx, tab_id, deadline, "after ScrollTo to input");
  let _scroll = recv_scroll_state_updated(&ui_rx, tab_id, deadline, "after ScrollTo frame");
  assert_eq!(
    // Sample well within the box so minor focus-scroll adjustments don't move it out from under us.
    rgba_at_css(&frame, 10, 130),
    [255, 0, 0, 255],
    "expected initial number-input value=0 (red box) after scrolling into view"
  );
  assert!(
    frame.scroll_state.viewport.y > 0.0,
    "expected scroll_y to be > 0 after ScrollTo so wheel-up would normally scroll; got {}",
    frame.scroll_state.viewport.y
  );

  // Drain any queued messages (scroll state update, hover updates, etc) so assertions are scoped to
  // the focus + wheel gesture.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  // Click the input body (not the spinner) to focus it.
  ui_tx
    .send(support::pointer_down(tab_id, (10.0, 50.0), PointerButton::Primary))
    .expect("pointer down");
  ui_tx
    .send(support::pointer_up(tab_id, (10.0, 50.0), PointerButton::Primary))
    .expect("pointer up");

  // Wait for the focus repaint. Clicking a control can trigger focus-scroll with padding, so use
  // the post-focus frame's scroll position as our baseline for the wheel gesture.
  let deadline = Instant::now() + TIMEOUT;
  let frame = recv_until_frame_ready(&ui_rx, tab_id, deadline, "after focusing number input");
  let _scroll = recv_scroll_state_updated(&ui_rx, tab_id, deadline, "after focusing number input frame");
  assert_eq!(
    rgba_at_css(&frame, 10, 130),
    [255, 0, 0, 255],
    "expected value=0 (red box) after focusing number input"
  );
  let baseline_scroll_y = frame.scroll_state.viewport.y;
  assert!(
    baseline_scroll_y > 0.0,
    "expected scroll_y to remain > 0 after focusing; got {baseline_scroll_y}"
  );

  // Drain any queued messages so assertions are scoped to the wheel event.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  // Simulate wheel-up over the focused number input.
  ui_tx
    .send(support::scroll_msg(
      tab_id,
      (0.0, -40.0),
      Some((10.0, 50.0)),
    ))
    .expect("wheel scroll over number input");

  let deadline = Instant::now() + TIMEOUT;
  let frame = recv_until_pixel(&ui_rx, tab_id, (10, 130), [0, 255, 0, 255], deadline);
  assert_eq!(rgba_at_css(&frame, 10, 130), [0, 255, 0, 255]);
  assert!(
    (frame.scroll_state.viewport.y - baseline_scroll_y).abs() < 1e-3,
    "expected wheel over focused number input to not scroll the viewport (baseline y={baseline_scroll_y}, got y={})",
    frame.scroll_state.viewport.y
  );

  let scroll = recv_scroll_state_updated(&ui_rx, tab_id, deadline, "after wheel frame");
  assert!(
    (scroll.viewport.y - baseline_scroll_y).abs() < 1e-3,
    "expected ScrollStateUpdated to preserve viewport scroll (baseline y={baseline_scroll_y}, got y={})",
    scroll.viewport.y
  );

  drop(ui_tx);
  join.join().expect("worker join");
}
