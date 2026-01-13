#![cfg(feature = "browser_ui")]

use fastrender::interaction::KeyAction;
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use tempfile::tempdir;

use super::support::{
  create_tab_msg, key_action, navigate_msg, recv_for_tab, scroll_msg, viewport_changed_msg,
  DEFAULT_TIMEOUT,
};

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

  // Wait for an initial frame so the worker has cached layout artifacts.
  recv_for_tab(&ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("expected initial FrameReady");

  // Wheel scroll inside the scroller element.
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 80.0), Some((10.0, 10.0))))
    .expect("Scroll");

  let msg = recv_for_tab(&ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| match msg {
    WorkerToUi::ScrollStateUpdated { scroll, .. } => {
      scroll.viewport.y.abs() < 1e-3 && scroll.elements.values().any(|pt| pt.y > 0.0)
    }
    _ => false,
  })
  .expect("expected ScrollStateUpdated after wheel scroll");
  let WorkerToUi::ScrollStateUpdated { scroll, .. } = msg else {
    unreachable!();
  };

  assert!(
    scroll.viewport.y.abs() < 1e-3,
    "expected wheel scroll over overflow:hidden scroller to not scroll the viewport (got {:?})",
    scroll.viewport
  );
  assert!(
    scroll.elements.values().any(|pt| pt.y > 0.0),
    "expected wheel scroll over overflow:hidden scroller to update element scroll offsets, got {:?}",
    scroll
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

  // Wait for an initial frame so focus traversal has cached layout artifacts.
  recv_for_tab(&ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("expected initial FrameReady");

  ui_tx.send(key_action(tab_id, KeyAction::Tab)).expect("Tab");

  let msg = recv_for_tab(&ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| match msg {
    WorkerToUi::ScrollStateUpdated { scroll, .. } => {
      scroll.viewport.y.abs() < 1e-3 && scroll.elements.values().any(|pt| pt.y > 0.0)
    }
    _ => false,
  })
  .expect("expected ScrollStateUpdated after focus scroll");
  let WorkerToUi::ScrollStateUpdated { scroll, .. } = msg else {
    unreachable!();
  };

  assert!(
    scroll.viewport.y.abs() < 1e-3,
    "expected focus scroll to adjust the overflow:hidden scroller, not the viewport (got {:?})",
    scroll.viewport
  );

  let scroll_y = scroll
    .elements
    .values()
    .copied()
    .map(|p| p.y)
    .fold(0.0_f32, f32::max);
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
