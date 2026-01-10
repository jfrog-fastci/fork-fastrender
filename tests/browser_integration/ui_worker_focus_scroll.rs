#![cfg(feature = "browser_ui")]

use fastrender::interaction::KeyAction;
use fastrender::ui::messages::{NavigationReason, PointerButton, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use tempfile::tempdir;

use super::support::{
  create_tab_msg, key_action, navigate_msg, pointer_down, pointer_up, viewport_changed_msg, DEFAULT_TIMEOUT,
};

fn wait_for_frame(rx: &Receiver<WorkerToUi>, tab_id: TabId, timeout: Duration) -> fastrender::ui::messages::RenderedFrame {
  let deadline = Instant::now() + timeout;
  loop {
    let remaining = deadline
      .checked_duration_since(Instant::now())
      .unwrap_or(Duration::from_secs(0));
    assert!(remaining > Duration::ZERO, "timed out waiting for FrameReady");
    let msg = rx.recv_timeout(remaining).expect("worker msg");
    if let WorkerToUi::FrameReady { tab_id: got, frame } = msg {
      if got == tab_id {
        return frame;
      }
    }
  }
}

#[test]
fn tab_focus_scrolls_viewport_to_reveal_focused_element() {
  let _lock = super::stage_listener_test_lock();

  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          body { height: 2000px; background: rgb(0,0,0); position: relative; }
          #target {
            position: absolute;
            left: 10px;
            top: 1500px;
            width: 120px;
            height: 30px;
            margin: 0;
            padding: 0;
            border: 0;
            background: rgb(255,0,0);
          }
        </style>
      </head>
      <body>
        <input id="target" value="hello" />
      </body>
    </html>
  "#;
  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = format!("file://{}/index.html", dir.path().display());

  let handle = spawn_ui_worker("fastr-ui-worker-focus-scroll").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 200), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let frame = wait_for_frame(&ui_rx, tab_id, DEFAULT_TIMEOUT);
  assert_eq!(
    frame.scroll_state.viewport.y, 0.0,
    "expected initial scroll position to be at top"
  );

  ui_tx.send(key_action(tab_id, KeyAction::Tab)).expect("Tab");

  // The focused input is at 1500px; tabbing to it should scroll the viewport down so it becomes
  // visible.
  let frame = {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
      let remaining = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::from_secs(0));
      assert!(remaining > Duration::ZERO, "timed out waiting for focused scroll frame");
      let msg = ui_rx.recv_timeout(remaining).expect("worker msg");
      match msg {
        WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => {
          if frame.scroll_state.viewport.y > 0.0 {
            break frame;
          }
        }
        _ => {}
      }
    }
  };

  let scroll_y = frame.scroll_state.viewport.y;
  assert!(scroll_y.is_finite() && scroll_y > 0.0, "expected scroll y > 0, got {scroll_y}");

  let viewport_top = scroll_y;
  let viewport_bottom = scroll_y + 200.0;
  let input_top = 1500.0;
  let input_bottom = 1500.0 + 30.0;
  assert!(
    viewport_top <= input_top && viewport_bottom >= input_bottom,
    "expected focused input [{input_top}, {input_bottom}] to be visible in viewport [{viewport_top}, {viewport_bottom}]",
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn tab_focus_scrolls_nested_scroller_to_reveal_focused_element() {
  let _lock = super::stage_listener_test_lock();

  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #scroller {
            width: 200px;
            height: 100px;
            overflow-y: scroll;
            border: 0;
            background: rgb(0,0,0);
          }
          #content {
            position: relative;
            height: 1000px;
          }
          #target {
            position: absolute;
            left: 10px;
            top: 800px;
            width: 120px;
            height: 30px;
            margin: 0;
            padding: 0;
            border: 0;
            background: rgb(255,0,0);
          }
        </style>
      </head>
      <body>
        <div id="scroller">
          <div id="content">
            <input id="target" value="hello" />
          </div>
        </div>
      </body>
    </html>
  "#;
  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = format!("file://{}/index.html", dir.path().display());

  let handle = spawn_ui_worker("fastr-ui-worker-focus-scroll-nested").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (220, 220), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let frame = wait_for_frame(&ui_rx, tab_id, DEFAULT_TIMEOUT);
  assert_eq!(
    frame.scroll_state.viewport.y, 0.0,
    "expected initial viewport scroll position to be at top"
  );
  assert!(
    frame.scroll_state.elements.is_empty(),
    "expected initial element scroll offsets to be empty"
  );

  ui_tx.send(key_action(tab_id, KeyAction::Tab)).expect("Tab");

  // The focused input is inside a nested scroll container at y=800; tabbing to it should scroll
  // the *element* scroller (not the viewport) enough for it to be visible.
  let frame = {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
      let remaining = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::from_secs(0));
      assert!(remaining > Duration::ZERO, "timed out waiting for focused scroll frame");
      let msg = ui_rx.recv_timeout(remaining).expect("worker msg");
      match msg {
        WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => {
          if frame.scroll_state.elements.values().any(|offset| offset.y > 0.0) {
            break frame;
          }
        }
        _ => {}
      }
    }
  };

  assert_eq!(
    frame.scroll_state.viewport.y, 0.0,
    "expected focus scroll to adjust the nested scroller, not the viewport"
  );
  assert_eq!(
    frame.scroll_state.elements.len(),
    1,
    "expected exactly one element scroller to be updated"
  );

  let scroll_y = frame
    .scroll_state
    .elements
    .values()
    .next()
    .copied()
    .expect("element scroll offset")
    .y;
  assert!(scroll_y.is_finite() && scroll_y > 0.0, "expected element scroll y > 0, got {scroll_y}");

  let viewport_top = scroll_y;
  let viewport_bottom = scroll_y + 100.0;
  let input_top = 800.0;
  let input_bottom = 800.0 + 30.0;
  assert!(
    viewport_top <= input_top && viewport_bottom >= input_bottom,
    "expected focused input [{input_top}, {input_bottom}] to be visible in nested scrollport [{viewport_top}, {viewport_bottom}]",
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn click_focus_scrolls_nested_scroller_to_reveal_focused_element() {
  let _lock = super::stage_listener_test_lock();

  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #scroller {
            width: 200px;
            height: 100px;
            overflow-y: scroll;
            border: 0;
            background: rgb(0,0,0);
          }
          #content {
            position: relative;
            height: 1000px;
          }
          #target {
            position: absolute;
            left: 10px;
            top: 90px;
            width: 120px;
            height: 30px;
            margin: 0;
            padding: 0;
            border: 0;
            background: rgb(255,0,0);
          }
        </style>
      </head>
      <body>
        <div id="scroller">
          <div id="content">
            <input id="target" value="hello" />
          </div>
        </div>
      </body>
    </html>
  "#;
  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = format!("file://{}/index.html", dir.path().display());

  let handle = spawn_ui_worker("fastr-ui-worker-focus-scroll-click-nested").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (220, 220), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let frame = wait_for_frame(&ui_rx, tab_id, DEFAULT_TIMEOUT);
  assert!(
    frame.scroll_state.elements.is_empty(),
    "expected initial element scroll offsets to be empty"
  );

  // The input starts at y=90 within the 100px scrollport, so it is partially clipped. Clicking the
  // visible portion should focus it and scroll the nested scroller just enough so it becomes fully
  // visible.
  ui_tx
    .send(pointer_down(
      tab_id,
      (20.0, 95.0),
      PointerButton::Primary,
    ))
    .expect("PointerDown");
  ui_tx
    .send(pointer_up(
      tab_id,
      (20.0, 95.0),
      PointerButton::Primary,
    ))
    .expect("PointerUp");

  let frame = {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
      let remaining = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::from_secs(0));
      assert!(remaining > Duration::ZERO, "timed out waiting for focused scroll frame");
      let msg = ui_rx.recv_timeout(remaining).expect("worker msg");
      match msg {
        WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => {
          if frame.scroll_state.elements.values().any(|offset| offset.y > 0.0) {
            break frame;
          }
        }
        _ => {}
      }
    }
  };

  assert_eq!(frame.scroll_state.elements.len(), 1);
  let scroll_y = frame
    .scroll_state
    .elements
    .values()
    .next()
    .copied()
    .expect("element scroll offset")
    .y;
  assert!(scroll_y.is_finite() && scroll_y > 0.0, "expected element scroll y > 0, got {scroll_y}");

  let viewport_top = scroll_y;
  let viewport_bottom = scroll_y + 100.0;
  let input_top = 90.0;
  let input_bottom = 90.0 + 30.0;
  assert!(
    viewport_top <= input_top && viewport_bottom >= input_bottom,
    "expected focused input [{input_top}, {input_bottom}] to be visible in nested scrollport [{viewport_top}, {viewport_bottom}]",
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn tab_focus_scrolls_viewport_and_nested_scroller_when_scroller_is_below_fold() {
  let _lock = super::stage_listener_test_lock();

  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #spacer { height: 1500px; }
          #scroller {
            width: 200px;
            height: 100px;
            overflow-y: scroll;
            border: 0;
            background: rgb(0,0,0);
          }
          #content {
            position: relative;
            height: 1000px;
          }
          #target {
            position: absolute;
            left: 10px;
            top: 800px;
            width: 120px;
            height: 30px;
            margin: 0;
            padding: 0;
            border: 0;
            background: rgb(255,0,0);
          }
        </style>
      </head>
      <body>
        <div id="spacer"></div>
        <div id="scroller">
          <div id="content">
            <input id="target" value="hello" />
          </div>
        </div>
      </body>
    </html>
  "#;
  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = format!("file://{}/index.html", dir.path().display());

  let handle =
    spawn_ui_worker("fastr-ui-worker-focus-scroll-scroller-below-fold").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (220, 220), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let frame = wait_for_frame(&ui_rx, tab_id, DEFAULT_TIMEOUT);
  assert_eq!(
    frame.scroll_state.viewport.y, 0.0,
    "expected initial viewport scroll position to be at top"
  );
  assert!(
    frame.scroll_state.elements.is_empty(),
    "expected initial element scroll offsets to be empty"
  );

  ui_tx.send(key_action(tab_id, KeyAction::Tab)).expect("Tab");

  // The focused input is inside a scroll container that is itself below the fold (y=1500). Focus
  // scrolling should:
  // 1) scroll the nested scroller so the input is visible in its scrollport, and
  // 2) scroll the document viewport so the scroller (and focused input) become visible.
  let frame = {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
      let remaining = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::from_secs(0));
      assert!(remaining > Duration::ZERO, "timed out waiting for focused scroll frame");
      let msg = ui_rx.recv_timeout(remaining).expect("worker msg");
      match msg {
        WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => {
          if frame.scroll_state.viewport.y > 0.0
            && frame.scroll_state.elements.values().any(|offset| offset.y > 0.0)
          {
            break frame;
          }
        }
        _ => {}
      }
    }
  };

  assert!(
    frame.scroll_state.viewport.y > 0.0,
    "expected viewport to scroll down to the scroller below the fold"
  );
  assert_eq!(
    frame.scroll_state.elements.len(),
    1,
    "expected exactly one element scroller to be updated"
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

  // Validate the target is visible inside the scroller itself (100px tall).
  let scroller_top = element_scroll_y;
  let scroller_bottom = element_scroll_y + 100.0;
  let input_top_in_scroller = 800.0;
  let input_bottom_in_scroller = 800.0 + 30.0;
  assert!(
    scroller_top <= input_top_in_scroller && scroller_bottom >= input_bottom_in_scroller,
    "expected focused input [{input_top_in_scroller}, {input_bottom_in_scroller}] to be visible in nested scrollport [{scroller_top}, {scroller_bottom}]",
  );

  // Validate the focused input is visible in the document viewport (220px tall) after combining
  // viewport + element scroll offsets.
  let viewport_scroll_y = frame.scroll_state.viewport.y;
  let viewport_top = viewport_scroll_y;
  let viewport_bottom = viewport_scroll_y + 220.0;
  let input_abs_top = 1500.0 + input_top_in_scroller - element_scroll_y;
  let input_abs_bottom = input_abs_top + 30.0;
  assert!(
    viewport_top <= input_abs_top && viewport_bottom >= input_abs_bottom,
    "expected focused input [{input_abs_top}, {input_abs_bottom}] to be visible in viewport [{viewport_top}, {viewport_bottom}]",
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
