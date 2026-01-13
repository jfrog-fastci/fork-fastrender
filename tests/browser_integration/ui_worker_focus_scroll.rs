#![cfg(feature = "browser_ui")]

use fastrender::interaction::KeyAction;
use fastrender::ui::messages::{NavigationReason, PointerButton, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use fastrender::Point;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use tempfile::tempdir;

use super::support::{
  create_tab_msg, key_action, navigate_msg, pointer_down, pointer_up, scroll_to_msg,
  viewport_changed_msg, DEFAULT_TIMEOUT,
};

fn wait_for_frame(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  timeout: Duration,
) -> fastrender::ui::messages::RenderedFrame {
  let deadline = Instant::now() + timeout;
  loop {
    let remaining = deadline
      .checked_duration_since(Instant::now())
      .unwrap_or(Duration::from_secs(0));
    assert!(
      remaining > Duration::ZERO,
      "timed out waiting for FrameReady"
    );
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
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
  let url = url::Url::from_file_path(dir.path().join("index.html"))
    .unwrap()
    .to_string();

  let handle = spawn_ui_worker("fastr-ui-worker-focus-scroll").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
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
      assert!(
        remaining > Duration::ZERO,
        "timed out waiting for focused scroll frame"
      );
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
  assert!(
    scroll_y.is_finite() && scroll_y > 0.0,
    "expected scroll y > 0, got {scroll_y}"
  );
  assert!(
    frame.scroll_state.viewport_delta.y > 0.0,
    "expected viewport_delta.y > 0 after focus scroll, got {:?}",
    frame.scroll_state.viewport_delta
  );
  assert!(
    frame.scroll_state.elements_delta.is_empty(),
    "expected focus viewport scroll to not report element deltas, got {:?}",
    frame.scroll_state.elements_delta
  );

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
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
  let url = url::Url::from_file_path(dir.path().join("index.html"))
    .unwrap()
    .to_string();

  let handle = spawn_ui_worker("fastr-ui-worker-focus-scroll-nested").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
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
      assert!(
        remaining > Duration::ZERO,
        "timed out waiting for focused scroll frame"
      );
      let msg = ui_rx.recv_timeout(remaining).expect("worker msg");
      match msg {
        WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => {
          if frame
            .scroll_state
            .elements
            .values()
            .any(|offset| offset.y > 0.0)
          {
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
  assert!(
    scroll_y.is_finite() && scroll_y > 0.0,
    "expected element scroll y > 0, got {scroll_y}"
  );
  assert_eq!(
    frame.scroll_state.viewport_delta,
    Point::ZERO,
    "expected element focus scroll to not change viewport_delta, got {:?}",
    frame.scroll_state.viewport_delta
  );
  assert_eq!(
    frame.scroll_state.elements_delta.len(),
    1,
    "expected exactly one element delta after focus scroll, got {:?}",
    frame.scroll_state.elements_delta
  );
  let delta_y = frame
    .scroll_state
    .elements_delta
    .values()
    .next()
    .copied()
    .expect("element scroll delta")
    .y;
  assert!(
    delta_y.is_finite() && delta_y > 0.0,
    "expected element scroll delta y > 0 after focus scroll, got {delta_y}"
  );

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
fn tab_focus_scrolls_bordered_scroller_to_expected_offset() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
            overflow-x: scroll;
            overflow-y: scroll;
            scrollbar-gutter: stable both-edges;
            scrollbar-width: auto;
            border: 10px solid rgb(0,0,0);
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
  let url = url::Url::from_file_path(dir.path().join("index.html"))
    .unwrap()
    .to_string();

  let handle =
    spawn_ui_worker("fastr-ui-worker-focus-scroll-bordered-scroller").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (220, 220), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let frame = wait_for_frame(&ui_rx, tab_id, DEFAULT_TIMEOUT);
  assert_eq!(frame.scroll_state.viewport.y, 0.0);
  assert!(
    frame.scroll_state.elements.is_empty(),
    "expected initial element scroll offsets to be empty"
  );

  ui_tx.send(key_action(tab_id, KeyAction::Tab)).expect("Tab");

  // Wait for the scroller to receive a non-zero scroll offset.
  let frame = {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
      let remaining = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::from_secs(0));
      assert!(
        remaining > Duration::ZERO,
        "timed out waiting for focused scroll frame"
      );
      let msg = ui_rx.recv_timeout(remaining).expect("worker msg");
      match msg {
        WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => {
          if frame
            .scroll_state
            .elements
            .values()
            .any(|offset| offset.y > 0.0)
          {
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
  assert!(
    scroll_y.is_finite() && scroll_y > 0.0,
    "expected element scroll y > 0, got {scroll_y}"
  );

  // Focus-driven auto-scroll should use the *scrollport* coordinate space for the bordered scroller
  // (padding box minus reserved scrollbar gutters), not the border box. Borders and gutters change
  // the scrollport origin/size but must not affect the meaning of `scrollTop=0`.
  let hide_scrollbars = std::env::var("FASTR_HIDE_SCROLLBARS")
    .ok()
    .map(|v| {
      let lower = v.trim().to_ascii_lowercase();
      !matches!(lower.as_str(), "0" | "false" | "no" | "off")
    })
    .unwrap_or(false);
  let gutter = if hide_scrollbars { 0.0 } else { 15.0 };
  let focus_padding = 8.0;
  let scrollport_height = 100.0 - gutter * 2.0;
  let input_top = 800.0;
  let input_bottom = input_top + 30.0;
  let expected = input_bottom - (scrollport_height - focus_padding);
  assert!(
    (scroll_y - expected).abs() <= 1.0,
    "expected bordered scroller scroll y ≈ {expected}, got {scroll_y}"
  );

  let viewport_top = scroll_y;
  let viewport_bottom = scroll_y + scrollport_height;
  assert!(
    viewport_top <= input_top && viewport_bottom >= input_bottom,
    "expected focused input [{input_top}, {input_bottom}] to be visible in nested scrollport [{viewport_top}, {viewport_bottom}]",
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn tab_focus_scrolls_horizontal_scroller_to_reveal_focused_element() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
            overflow-x: scroll;
            overflow-y: hidden;
            border: 0;
            background: rgb(0,0,0);
          }
          #content {
            position: relative;
            width: 1200px;
            height: 100px;
          }
          #target {
            position: absolute;
            left: 800px;
            top: 10px;
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
  let url = url::Url::from_file_path(dir.path().join("index.html"))
    .unwrap()
    .to_string();

  let handle = spawn_ui_worker("fastr-ui-worker-focus-scroll-horizontal").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
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

  ui_tx.send(key_action(tab_id, KeyAction::Tab)).expect("Tab");

  // The focused input is at x=800 within a 200px wide horizontal scrollport; tabbing to it should
  // scroll the element scroller along the inline axis so it becomes visible.
  let frame = {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
      let remaining = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::from_secs(0));
      assert!(
        remaining > Duration::ZERO,
        "timed out waiting for focused scroll frame"
      );
      let msg = ui_rx.recv_timeout(remaining).expect("worker msg");
      match msg {
        WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => {
          if frame
            .scroll_state
            .elements
            .values()
            .any(|offset| offset.x > 0.0)
          {
            break frame;
          }
        }
        _ => {}
      }
    }
  };

  assert_eq!(
    frame.scroll_state.viewport.x, 0.0,
    "expected focus scroll to adjust the nested scroller, not the viewport"
  );
  assert_eq!(
    frame.scroll_state.viewport.y, 0.0,
    "expected focus scroll to adjust the nested scroller, not the viewport"
  );
  assert_eq!(
    frame.scroll_state.elements.len(),
    1,
    "expected exactly one element scroller to be updated"
  );

  let scroll_x = frame
    .scroll_state
    .elements
    .values()
    .next()
    .copied()
    .expect("element scroll offset")
    .x;
  assert!(
    scroll_x.is_finite() && scroll_x > 0.0,
    "expected element scroll x > 0, got {scroll_x}"
  );

  let viewport_left = scroll_x;
  let viewport_right = scroll_x + 200.0;
  let input_left = 800.0;
  let input_right = 800.0 + 120.0;
  assert!(
    viewport_left <= input_left && viewport_right >= input_right,
    "expected focused input [{input_left}, {input_right}] to be visible in nested scrollport [{viewport_left}, {viewport_right}]",
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn click_focus_scrolls_nested_scroller_to_reveal_focused_element() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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

  let handle =
    spawn_ui_worker("fastr-ui-worker-focus-scroll-click-nested").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
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
    .send(pointer_down(tab_id, (20.0, 95.0), PointerButton::Primary))
    .expect("PointerDown");
  ui_tx
    .send(pointer_up(tab_id, (20.0, 95.0), PointerButton::Primary))
    .expect("PointerUp");

  let frame = {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
      let remaining = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::from_secs(0));
      assert!(
        remaining > Duration::ZERO,
        "timed out waiting for focused scroll frame"
      );
      let msg = ui_rx.recv_timeout(remaining).expect("worker msg");
      match msg {
        WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => {
          if frame
            .scroll_state
            .elements
            .values()
            .any(|offset| offset.y > 0.0)
          {
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
  assert!(
    scroll_y.is_finite() && scroll_y > 0.0,
    "expected element scroll y > 0, got {scroll_y}"
  );

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
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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

  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
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
      assert!(
        remaining > Duration::ZERO,
        "timed out waiting for focused scroll frame"
      );
      let msg = ui_rx.recv_timeout(remaining).expect("worker msg");
      match msg {
        WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => {
          if frame.scroll_state.viewport.y > 0.0
            && frame
              .scroll_state
              .elements
              .values()
              .any(|offset| offset.y > 0.0)
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

#[test]
fn shift_tab_scrolls_viewport_back_up_to_reveal_previous_focus_target() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          body { height: 2000px; background: rgb(0,0,0); position: relative; }
          input {
            position: absolute;
            left: 10px;
            width: 120px;
            height: 30px;
            margin: 0;
            padding: 0;
            border: 0;
            background: rgb(255,0,0);
          }
          #top { top: 0px; }
          #bottom { top: 1500px; }
        </style>
      </head>
      <body>
        <input id="top" value="top" />
        <input id="bottom" value="bottom" />
      </body>
    </html>
  "#;
  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = format!("file://{}/index.html", dir.path().display());

  let handle = spawn_ui_worker("fastr-ui-worker-focus-scroll-shift-tab").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 200), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let frame = wait_for_frame(&ui_rx, tab_id, DEFAULT_TIMEOUT);
  assert_eq!(frame.scroll_state.viewport.y, 0.0);

  // Tab from no focus should focus the first input without needing to scroll.
  ui_tx
    .send(key_action(tab_id, KeyAction::Tab))
    .expect("Tab to top");
  let frame = wait_for_frame(&ui_rx, tab_id, DEFAULT_TIMEOUT);
  assert_eq!(
    frame.scroll_state.viewport.y, 0.0,
    "expected top input focus to keep viewport at y=0"
  );

  // Tab to the second input should scroll down.
  ui_tx
    .send(key_action(tab_id, KeyAction::Tab))
    .expect("Tab to bottom");
  let frame = {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
      let remaining = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::from_secs(0));
      assert!(
        remaining > Duration::ZERO,
        "timed out waiting for bottom focus scroll"
      );
      let msg = ui_rx.recv_timeout(remaining).expect("worker msg");
      if let WorkerToUi::FrameReady { tab_id: got, frame } = msg {
        if got == tab_id && frame.scroll_state.viewport.y > 0.0 {
          break frame;
        }
      }
    }
  };
  let bottom_scroll_y = frame.scroll_state.viewport.y;
  assert!(bottom_scroll_y > 0.0);

  // Shift+Tab should move focus back to the first input and scroll up to reveal it.
  ui_tx
    .send(key_action(tab_id, KeyAction::ShiftTab))
    .expect("ShiftTab to top");
  let frame = {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
      let remaining = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::from_secs(0));
      assert!(
        remaining > Duration::ZERO,
        "timed out waiting for top focus scroll"
      );
      let msg = ui_rx.recv_timeout(remaining).expect("worker msg");
      if let WorkerToUi::FrameReady { tab_id: got, frame } = msg {
        if got == tab_id && frame.scroll_state.viewport.y < bottom_scroll_y {
          break frame;
        }
      }
    }
  };
  assert!(
    frame.scroll_state.viewport.y <= 8.0,
    "expected viewport to scroll back near top (got {})",
    frame.scroll_state.viewport.y
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn tab_focus_scrolls_oversized_focus_target_to_nearest_edge_in_viewport() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
            top: 1000px;
            width: 180px;
            height: 400px;
            margin: 0;
            padding: 0;
            border: 0;
            background: rgb(255,0,0);
          }
        </style>
      </head>
      <body>
        <textarea id="target"></textarea>
      </body>
    </html>
  "#;
  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = url::Url::from_file_path(dir.path().join("index.html"))
    .unwrap()
    .to_string();

  let handle =
    spawn_ui_worker("fastr-ui-worker-focus-scroll-oversized").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
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

  // The focused textarea is much taller than the viewport. Focus scrolling should still move the
  // viewport so the *nearest edge* becomes visible, instead of becoming a no-op or aligning the far
  // edge (which would overscroll past the start).
  let frame = {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
      let remaining = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::from_secs(0));
      assert!(
        remaining > Duration::ZERO,
        "timed out waiting for oversized focused scroll frame"
      );
      let msg = ui_rx.recv_timeout(remaining).expect("worker msg");
      if let WorkerToUi::FrameReady { tab_id: got, frame } = msg {
        if got == tab_id && frame.scroll_state.viewport.y > 0.0 {
          break frame;
        }
      }
    }
  };

  let scroll_y = frame.scroll_state.viewport.y;
  assert!(
    scroll_y.is_finite() && scroll_y > 0.0,
    "expected scroll y > 0, got {scroll_y}"
  );

  let target_top = 1000.0;
  let target_bottom = 1000.0 + 400.0;
  let viewport_height = 200.0;
  assert!(
    scroll_y < (target_bottom - viewport_height),
    "expected focus scroll to choose the nearest edge (start), not align the far end; got scroll_y={scroll_y}"
  );

  let viewport_top = scroll_y;
  let viewport_bottom = scroll_y + viewport_height;
  assert!(
    viewport_top <= target_top + 1.0 && viewport_bottom >= target_top - 1.0,
    "expected target top {target_top} to be visible in viewport [{viewport_top}, {viewport_bottom}]",
  );
  assert!(
    target_top - viewport_top <= 32.0,
    "expected target top to be near the viewport start (got offset {})",
    target_top - viewport_top
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn tab_focus_scrolls_oversized_target_when_it_spans_both_viewport_edges() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
            top: 1000px;
            width: 180px;
            height: 400px;
            margin: 0;
            padding: 0;
            border: 0;
            background: rgb(255,0,0);
          }
        </style>
      </head>
      <body>
        <textarea id="target"></textarea>
      </body>
    </html>
  "#;
  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = url::Url::from_file_path(dir.path().join("index.html"))
    .unwrap()
    .to_string();

  let handle = spawn_ui_worker("fastr-ui-worker-focus-scroll-oversized-span")
    .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 200), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let frame = wait_for_frame(&ui_rx, tab_id, DEFAULT_TIMEOUT);
  assert_eq!(frame.scroll_state.viewport.y, 0.0);

  // Scroll the viewport to show the *middle* of the oversized target: its top edge is above the
  // viewport and its bottom edge is below the viewport.
  ui_tx
    .send(scroll_to_msg(tab_id, (0.0, 1090.0)))
    .expect("ScrollTo");
  let frame = {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
      let remaining = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::from_secs(0));
      assert!(
        remaining > Duration::ZERO,
        "timed out waiting for pre-scroll frame"
      );
      let msg = ui_rx.recv_timeout(remaining).expect("worker msg");
      if let WorkerToUi::FrameReady { tab_id: got, frame } = msg {
        if got == tab_id && frame.scroll_state.viewport.y > 0.0 {
          break frame;
        }
      }
    }
  };
  let pre_scroll_y = frame.scroll_state.viewport.y;
  assert!(
    pre_scroll_y > 0.0,
    "expected viewport to be pre-scrolled, got {pre_scroll_y}"
  );

  ui_tx.send(key_action(tab_id, KeyAction::Tab)).expect("Tab");

  // Before the fix, the oversized element spanning both edges could cause a no-op focus scroll.
  // With "nearest" semantics, focusing it should scroll to the nearest edge (here: the start/top).
  let frame = {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
      let remaining = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::from_secs(0));
      assert!(
        remaining > Duration::ZERO,
        "timed out waiting for focused scroll frame"
      );
      let msg = ui_rx.recv_timeout(remaining).expect("worker msg");
      if let WorkerToUi::FrameReady { tab_id: got, frame } = msg {
        if got == tab_id && (frame.scroll_state.viewport.y - pre_scroll_y).abs() > 1.0 {
          break frame;
        }
      }
    }
  };

  let scroll_y = frame.scroll_state.viewport.y;
  assert!(
    scroll_y.is_finite() && scroll_y > 0.0,
    "expected scroll y > 0, got {scroll_y}"
  );
  assert!(
    scroll_y < pre_scroll_y - 1.0,
    "expected focus scroll to move up toward the target start edge (pre={pre_scroll_y}, post={scroll_y})"
  );

  let target_top = 1000.0;
  let viewport_top = scroll_y;
  let viewport_bottom = scroll_y + 200.0;
  assert!(
    viewport_top <= target_top + 1.0 && viewport_bottom >= target_top - 1.0,
    "expected target top {target_top} to be visible in viewport [{viewport_top}, {viewport_bottom}]",
  );
  assert!(
    target_top - viewport_top <= 32.0,
    "expected target top to be near the viewport start (got offset {})",
    target_top - viewport_top
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
