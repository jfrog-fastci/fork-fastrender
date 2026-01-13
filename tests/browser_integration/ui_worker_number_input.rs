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
  rx: &fastrender::ui::WorkerToUiInbox,
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
    if let Some(msg) = support::recv_for_tab(
      rx,
      tab_id,
      remaining.min(Duration::from_millis(200)),
      |msg| matches!(msg, WorkerToUi::FrameReady { .. }),
    ) {
      if let WorkerToUi::FrameReady { frame, .. } = msg {
        return frame;
      }
    }
  }
}

fn recv_until_pixel(
  rx: &fastrender::ui::WorkerToUiInbox,
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
fn number_input_spinner_click_steps_value_and_repaints() {
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
      input[value="1"] ~ #box { background: rgb(0, 255, 0); }
    </style>
  </head>
  <body>
    <input id="n" type="number" value="0">
    <div id="box"></div>
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-number-input-spinner").expect("spawn ui worker");
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

  // Drain queued messages so assertions are scoped to the spinner click.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  // Click the spinner upper half: near the right edge of the input.
  ui_tx
    .send(support::pointer_down(
      tab_id,
      (110.0, 8.0),
      PointerButton::Primary,
    ))
    .expect("pointer down");
  ui_tx
    .send(support::pointer_up(
      tab_id,
      (110.0, 8.0),
      PointerButton::Primary,
    ))
    .expect("pointer up");

  let deadline = Instant::now() + TIMEOUT;
  let frame = recv_until_pixel(&ui_rx, tab_id, (10, 70), [0, 255, 0, 255], deadline);
  assert_eq!(rgba_at_css(&frame, 10, 70), [0, 255, 0, 255]);

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn number_input_arrow_keys_step_value_and_repaint() {
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
      input[value="1"] ~ #box { background: rgb(0, 255, 0); }
    </style>
  </head>
  <body>
    <input id="n" type="number" value="0">
    <div id="box"></div>
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-number-input-arrows").expect("spawn ui worker");
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

  // Click the input body (not the spinner) to focus it.
  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("pointer down");
  ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("pointer up");

  // Step up.
  ui_tx
    .send(support::key_action(tab_id, KeyAction::ArrowUp))
    .expect("key action");

  let deadline = Instant::now() + TIMEOUT;
  let frame = recv_until_pixel(&ui_rx, tab_id, (10, 70), [0, 255, 0, 255], deadline);
  assert_eq!(rgba_at_css(&frame, 10, 70), [0, 255, 0, 255]);

  // Step down.
  ui_tx
    .send(support::key_action(tab_id, KeyAction::ArrowDown))
    .expect("key action");

  let deadline = Instant::now() + TIMEOUT;
  let frame = recv_until_pixel(&ui_rx, tab_id, (10, 70), [255, 0, 0, 255], deadline);
  assert_eq!(rgba_at_css(&frame, 10, 70), [255, 0, 0, 255]);

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn number_input_wheel_scroll_steps_value_and_repaints_without_scrolling_page() {
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
      input[value="1"] ~ #box { background: rgb(0, 255, 0); }
      /* Make the page scrollable so an unhandled wheel would scroll the viewport. */
      #spacer { height: 2000px; }
    </style>
  </head>
  <body>
    <input id="n" type="number" value="1" min="0">
    <div id="box"></div>
    <div id="spacer"></div>
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
  let frame = recv_until_frame_ready(&ui_rx, tab_id, deadline);
  assert_eq!(frame.scroll_state.viewport.y, 0.0);
  assert_eq!(rgba_at_css(&frame, 10, 70), [0, 255, 0, 255]);

  // Drain queued messages so assertions are scoped to focus + wheel stepping.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  // Focus the input body (not the spinner) so wheel-stepping is enabled.
  ui_tx
    .send(support::pointer_down(tab_id, (10.0, 10.0), PointerButton::Primary))
    .expect("pointer down");
  ui_tx
    .send(support::pointer_up(tab_id, (10.0, 10.0), PointerButton::Primary))
    .expect("pointer up");

  // Wheel down over the focused input: should step value 1 → 0 and *not* scroll the page.
  ui_tx
    .send(support::scroll_msg(tab_id, (0.0, 40.0), Some((10.0, 10.0))))
    .expect("scroll");

  let deadline = Instant::now() + TIMEOUT;
  let frame = recv_until_pixel(&ui_rx, tab_id, (10, 70), [255, 0, 0, 255], deadline);
  assert_eq!(rgba_at_css(&frame, 10, 70), [255, 0, 0, 255]);
  assert_eq!(
    frame.scroll_state.viewport.y, 0.0,
    "expected wheel-stepping number input to not scroll the page"
  );

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn number_input_page_up_down_keys_step_value_and_repaint() {
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
      input[value="10"] ~ #box { background: rgb(0, 255, 0); }
    </style>
  </head>
  <body>
    <input id="n" type="number" value="0">
    <div id="box"></div>
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-number-input-page-keys").expect("spawn ui worker");
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

  // Click the input body (not the spinner) to focus it.
  ui_tx
    .send(support::pointer_down(tab_id, (10.0, 10.0), PointerButton::Primary))
    .expect("pointer down");
  ui_tx
    .send(support::pointer_up(tab_id, (10.0, 10.0), PointerButton::Primary))
    .expect("pointer up");

  // PageUp should step by a larger delta than arrows.
  ui_tx
    .send(support::key_action(tab_id, KeyAction::PageUp))
    .expect("key action");

  let deadline = Instant::now() + TIMEOUT;
  let frame = recv_until_pixel(&ui_rx, tab_id, (10, 70), [0, 255, 0, 255], deadline);
  assert_eq!(rgba_at_css(&frame, 10, 70), [0, 255, 0, 255]);

  // PageDown should step back down.
  ui_tx
    .send(support::key_action(tab_id, KeyAction::PageDown))
    .expect("key action");

  let deadline = Instant::now() + TIMEOUT;
  let frame = recv_until_pixel(&ui_rx, tab_id, (10, 70), [255, 0, 0, 255], deadline);
  assert_eq!(rgba_at_css(&frame, 10, 70), [255, 0, 0, 255]);

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn number_input_step_affects_get_form_submission_value() {
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
      #n { position: absolute; left: 0; top: 60px; width: 120px; height: 24px; }
      #submit { position: absolute; left: 0; top: 0; width: 120px; height: 40px; }
    </style>
  </head>
  <body>
    <form action="result.html">
      <input id="n" type="number" name="n" value="0">
      <input id="submit" type="submit" value="Go">
    </form>
  </body>
</html>
"#,
  );
  let _result_url = site.write(
    "result.html",
    r#"<!doctype html>
<html><body>ok</body></html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-number-input-form").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (240, 160), 1.0))
    .expect("viewport");
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      page_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate");

  // Wait for the initial frame so hit testing works.
  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {page_url}"));

  // Drain any queued messages (navigation committed, loading state, etc) so assertions are scoped
  // to the submit click.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  // Focus the number input, then step it up.
  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 70.0),
      PointerButton::Primary,
    ))
    .expect("pointer down");
  ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 70.0),
      PointerButton::Primary,
    ))
    .expect("pointer up");
  ui_tx
    .send(support::key_action(tab_id, KeyAction::ArrowUp))
    .expect("key action");

  // Click submit.
  ui_tx
    .send(fastrender::ui::messages::UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: fastrender::ui::messages::PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("pointer down");
  ui_tx
    .send(fastrender::ui::messages::UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: fastrender::ui::messages::PointerModifiers::NONE,
    })
    .expect("pointer up");

  let mut expected = url::Url::parse(&page_url)
    .expect("parse page url")
    .join("result.html")
    .expect("resolve result.html");
  expected.set_query(Some("n=1"));
  let expected_url = expected.to_string();

  support::recv_for_tab(
    &ui_rx,
    tab_id,
    TIMEOUT,
    |msg| matches!(msg, WorkerToUi::NavigationStarted { url, .. } if url == &expected_url),
  )
  .unwrap_or_else(|| {
    let msgs = support::drain_for(&ui_rx, Duration::from_millis(200));
    panic!(
      "timed out waiting for NavigationStarted({expected_url}); saw:\n{}",
      support::format_messages(&msgs)
    );
  });

  support::recv_for_tab(
    &ui_rx,
    tab_id,
    TIMEOUT,
    |msg| matches!(msg, WorkerToUi::NavigationCommitted { url, .. } if url == &expected_url),
  )
  .unwrap_or_else(|| {
    let msgs = support::drain_for(&ui_rx, Duration::from_millis(200));
    panic!(
      "timed out waiting for NavigationCommitted({expected_url}); saw:\n{}",
      support::format_messages(&msgs)
    );
  });

  drop(ui_tx);
  join.join().expect("worker join");
}
