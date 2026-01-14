#![cfg(feature = "browser_ui")]

use fastrender::interaction::KeyAction;
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use fastrender::ui::WorkerToUiInbox;

use super::support::{
  create_tab_msg, key_action, navigate_msg, viewport_changed_msg, DEFAULT_TIMEOUT,
};

fn wait_for_frame(
  rx: &WorkerToUiInbox,
  tab_id: TabId,
  timeout: std::time::Duration,
  mut pred: impl FnMut(&fastrender::ui::messages::RenderedFrame) -> bool,
) -> fastrender::ui::messages::RenderedFrame {
  let msg = super::support::recv_until(rx, timeout, |msg| match msg {
    WorkerToUi::FrameReady { tab_id: got, frame } => *got == tab_id && pred(frame),
    _ => false,
  })
  .expect("timed out waiting for FrameReady");

  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("expected FrameReady, got {other:?}"),
  }
}

#[test]
fn keyboard_scroll_targets_focused_overflow_scroller() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = super::support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #scroller {
              width: 200px;
              height: 100px;
              overflow-y: auto;
              border: 0;
            }
            #content { height: 1000px; }
          </style>
        </head>
        <body>
          <div id="scroller" tabindex="0">
            <div id="content"></div>
          </div>
        </body>
      </html>
    "#,
  );

  let handle =
    spawn_ui_worker("fastr-ui-worker-keyboard-scroll-focused-scroller").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (220, 220), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  // Initial frame: no scroll offsets.
  let frame = wait_for_frame(&ui_rx, tab_id, DEFAULT_TIMEOUT, |_| true);
  assert!(
    frame.scroll_state.viewport.y.abs() < 1e-3,
    "expected viewport scroll y to start at 0, got {}",
    frame.scroll_state.viewport.y
  );
  assert!(
    frame.scroll_state.elements.is_empty(),
    "expected element scroll offsets to start empty"
  );

  // Focus the scroller (tabindex=0).
  ui_tx.send(key_action(tab_id, KeyAction::Tab)).expect("Tab");
  let _ = wait_for_frame(&ui_rx, tab_id, DEFAULT_TIMEOUT, |_| true);

  // ArrowDown should scroll the *element* scroller, not the viewport.
  ui_tx
    .send(key_action(tab_id, KeyAction::ArrowDown))
    .expect("ArrowDown");
  let frame = wait_for_frame(&ui_rx, tab_id, DEFAULT_TIMEOUT, |frame| {
    frame
      .scroll_state
      .elements
      .values()
      .any(|offset| offset.y > 0.0)
  });
  assert!(
    frame.scroll_state.viewport.y.abs() < 1.0,
    "expected viewport scroll y to remain ~0, got {}",
    frame.scroll_state.viewport.y
  );
  assert_eq!(
    frame.scroll_state.elements.len(),
    1,
    "expected exactly one element scroll container to be updated"
  );
  let element_scroll_y = frame
    .scroll_state
    .elements
    .values()
    .next()
    .copied()
    .expect("element scroll offset")
    .y;
  assert!(
    element_scroll_y.is_finite() && element_scroll_y > 0.0,
    "expected element scroll y > 0, got {element_scroll_y}"
  );

  // End should scroll the focused scroller near its max (1000 - 100 = 900).
  ui_tx.send(key_action(tab_id, KeyAction::End)).expect("End");
  let frame = wait_for_frame(&ui_rx, tab_id, DEFAULT_TIMEOUT, |frame| {
    frame
      .scroll_state
      .elements
      .values()
      .next()
      .is_some_and(|offset| offset.y > 850.0)
  });
  assert!(
    frame.scroll_state.viewport.y.abs() < 1.0,
    "expected viewport scroll y to remain ~0 after End, got {}",
    frame.scroll_state.viewport.y
  );
  let end_scroll_y = frame
    .scroll_state
    .elements
    .values()
    .next()
    .copied()
    .expect("element scroll offset")
    .y;
  assert!(
    end_scroll_y > 850.0,
    "expected End to scroll near max (~900), got {end_scroll_y}"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
