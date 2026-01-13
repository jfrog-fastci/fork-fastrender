#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  KeyAction, NavigationReason, PointerButton, RenderedFrame, TabId, WorkerToUi,
};
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
) -> RenderedFrame {
  loop {
    let now = Instant::now();
    if now >= deadline {
      let msgs = support::drain_for(rx, Duration::from_millis(200));
      panic!(
        "timed out waiting for FrameReady; saw:\n{}",
        support::format_messages(&msgs)
      );
    }
    let remaining = deadline.saturating_duration_since(now);
    if let Some(msg) = support::recv_for_tab(rx, tab_id, remaining.min(Duration::from_millis(200)), |msg| {
      matches!(msg, WorkerToUi::FrameReady { .. })
    }) {
      if let WorkerToUi::FrameReady { frame, .. } = msg {
        return frame;
      }
    }
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
    let frame = recv_until_frame_ready(rx, tab_id, deadline);
    let rgba = rgba_at_css(&frame, css_pos.0, css_pos.1);
    if rgba == expected {
      return frame;
    }
  }
}

#[test]
fn number_input_step_base_uses_current_value_when_min_missing() {
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
      #n { position: absolute; left: 0; top: 0; width: 120px; height: 32px; border: 0; padding: 0; }
      #box { position: absolute; left: 0; top: 64px; width: 64px; height: 64px; background: rgb(255, 0, 0); }
      input[value="3"] ~ #box { background: rgb(0, 255, 0); }
      input[value="-1"] ~ #box { background: rgb(0, 0, 255); }
    </style>
  </head>
  <body>
    <input id="n" type="number" step="2" value="1">
    <div id="box"></div>
  </body>
</html>
"#,
  );

  let handle =
    spawn_ui_worker("fastr-ui-worker-number-input-step-base-value").expect("spawn ui worker");
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
  let frame = recv_until_frame_ready(&ui_rx, tab_id, deadline);
  assert_eq!(rgba_at_css(&frame, 10, 70), [255, 0, 0, 255]);

  // Drain queued messages so assertions are scoped to the focus + key actions.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  // Focus the input body (not the spinner).
  ui_tx
    .send(support::pointer_down(tab_id, (10.0, 10.0), PointerButton::Primary))
    .expect("pointer down");
  ui_tx
    .send(support::pointer_up(tab_id, (10.0, 10.0), PointerButton::Primary))
    .expect("pointer up");

  // ArrowUp should step from 1 -> 3 when `min` is missing and step=2.
  ui_tx
    .send(support::key_action(tab_id, KeyAction::ArrowUp))
    .expect("key action");

  let deadline = Instant::now() + TIMEOUT;
  let frame = recv_until_pixel(&ui_rx, tab_id, (10, 70), [0, 255, 0, 255], deadline);
  assert_eq!(rgba_at_css(&frame, 10, 70), [0, 255, 0, 255]);

  // ArrowDown twice should step from 3 -> 1 -> -1 (still without min).
  ui_tx
    .send(support::key_action(tab_id, KeyAction::ArrowDown))
    .expect("key action");
  ui_tx
    .send(support::key_action(tab_id, KeyAction::ArrowDown))
    .expect("key action");

  let deadline = Instant::now() + TIMEOUT;
  let frame = recv_until_pixel(&ui_rx, tab_id, (10, 70), [0, 0, 255, 255], deadline);
  assert_eq!(rgba_at_css(&frame, 10, 70), [0, 0, 255, 255]);

  drop(ui_tx);
  join.join().expect("worker join");
}
