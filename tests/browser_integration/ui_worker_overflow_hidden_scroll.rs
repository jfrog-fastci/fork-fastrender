#![cfg(feature = "browser_ui")]

use fastrender::interaction::KeyAction;
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use tempfile::tempdir;

use super::support::{
  create_tab_msg, key_action, navigate_msg, scroll_msg, viewport_changed_msg, DEFAULT_TIMEOUT,
};

fn wait_for_frame(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  timeout: Duration,
  mut pred: impl FnMut(&fastrender::ui::messages::RenderedFrame) -> bool,
) -> fastrender::ui::messages::RenderedFrame {
  let deadline = Instant::now() + timeout;
  loop {
    let remaining = deadline
      .checked_duration_since(Instant::now())
      .unwrap_or(Duration::ZERO);
    assert!(
      remaining > Duration::ZERO,
      "timed out waiting for FrameReady for tab {tab_id:?}"
    );
    let msg = rx.recv_timeout(remaining).expect("worker msg");
    if let WorkerToUi::FrameReady { tab_id: got, frame } = msg {
      if got == tab_id && pred(&frame) {
        return frame;
      }
    }
  }
}

fn make_overflow_hidden_scroller_page() -> (tempfile::TempDir, String) {
  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #scroller {
            width: 120px;
            height: 60px;
            overflow: hidden;
            border: 1px solid black;
          }
          #scroller > .content {
            height: 400px;
            background: linear-gradient(#eee, #ccc);
          }
          .spacer { height: 2000px; }
        </style>
      </head>
      <body>
        <div id="scroller"><div class="content"></div></div>
        <div class="spacer"></div>
      </body>
    </html>
  "#;
  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = url::Url::from_file_path(dir.path().join("index.html"))
    .unwrap()
    .to_string();
  (dir, url)
}

fn make_overflow_hidden_focus_page() -> (tempfile::TempDir, String) {
  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #scroller {
            width: 200px;
            height: 100px;
            overflow: hidden;
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
          .spacer { height: 2000px; }
        </style>
      </head>
      <body>
        <div id="scroller">
          <div id="content">
            <input id="target" value="hello" />
          </div>
        </div>
        <div class="spacer"></div>
      </body>
    </html>
  "#;
  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = url::Url::from_file_path(dir.path().join("index.html"))
    .unwrap()
    .to_string();
  (dir, url)
}

#[test]
fn wheel_scroll_scrolls_overflow_hidden_scroller_under_pointer() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_overflow_hidden_scroller_page();

  let handle = spawn_ui_worker("fastr-ui-worker-overflow-hidden-wheel").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (240, 140), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let initial = wait_for_frame(&ui_rx, tab_id, DEFAULT_TIMEOUT, |_| true);
  assert!(
    initial.scroll_state.viewport.y.abs() < 1e-3,
    "expected initial viewport scroll to be 0, got {:?}",
    initial.scroll_state.viewport
  );

  // Wheel scroll inside the scroller element.
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 80.0), Some((10.0, 10.0))))
    .expect("Scroll");

  let initial_viewport = initial.scroll_state.viewport;
  let initial_elements = initial.scroll_state.elements.clone();
  let frame = wait_for_frame(&ui_rx, tab_id, DEFAULT_TIMEOUT, |frame| {
    frame.scroll_state.viewport != initial_viewport
      || frame.scroll_state.elements != initial_elements
  });

  assert!(
    (frame.scroll_state.viewport.y - initial_viewport.y).abs() < 1e-3,
    "expected wheel scroll over overflow:hidden scroller to not scroll the viewport (was {:?}, now {:?})",
    initial_viewport,
    frame.scroll_state.viewport
  );
  assert!(
    frame.scroll_state.elements.values().any(|pt| pt.y > 0.0),
    "expected wheel scroll over overflow:hidden scroller to update element scroll offsets, got {:?}",
    frame.scroll_state
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn tab_focus_scrolls_overflow_hidden_scroller_to_reveal_focused_element() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_overflow_hidden_focus_page();

  let handle =
    spawn_ui_worker("fastr-ui-worker-overflow-hidden-focus-scroll").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (240, 140), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let initial = wait_for_frame(&ui_rx, tab_id, DEFAULT_TIMEOUT, |_| true);
  assert!(
    initial.scroll_state.viewport.y.abs() < 1e-3,
    "expected initial viewport scroll to be 0, got {:?}",
    initial.scroll_state.viewport
  );
  assert!(
    initial.scroll_state.elements.is_empty(),
    "expected initial element scroll offsets to be empty, got {:?}",
    initial.scroll_state.elements
  );

  ui_tx.send(key_action(tab_id, KeyAction::Tab)).expect("Tab");

  let initial_viewport = initial.scroll_state.viewport;
  let initial_elements = initial.scroll_state.elements.clone();
  let frame = wait_for_frame(&ui_rx, tab_id, DEFAULT_TIMEOUT, |frame| {
    frame.scroll_state.viewport != initial_viewport
      || frame.scroll_state.elements != initial_elements
  });

  assert!(
    (frame.scroll_state.viewport.y - initial_viewport.y).abs() < 1e-3,
    "expected focus scroll to adjust the overflow:hidden scroller, not the viewport (was {:?}, now {:?})",
    initial_viewport,
    frame.scroll_state.viewport
  );
  assert!(
    frame.scroll_state.elements.len() == 1,
    "expected exactly one element scroller to be updated, got {:?}",
    frame.scroll_state.elements
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

  let scrollport_top = scroll_y;
  let scrollport_bottom = scroll_y + 100.0;
  let input_top = 800.0;
  let input_bottom = 800.0 + 30.0;
  assert!(
    scrollport_top <= input_top && scrollport_bottom >= input_bottom,
    "expected focused input [{input_top}, {input_bottom}] to be visible in overflow:hidden scrollport [{scrollport_top}, {scrollport_bottom}]",
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
