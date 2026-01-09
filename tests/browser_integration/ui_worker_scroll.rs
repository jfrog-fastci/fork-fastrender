#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{
  NavigationReason, PointerButton, RenderedFrame, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::worker::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use tempfile::tempdir;

use super::support::{create_tab_msg, navigate_msg, scroll_msg, viewport_changed_msg, DEFAULT_TIMEOUT};

fn sample_rgba_at_css(frame: &RenderedFrame, x_css: u32, y_css: u32) -> (u8, u8, u8, u8) {
  let x_px = ((x_css as f32) * frame.dpr).round() as u32;
  let y_px = ((y_css as f32) * frame.dpr).round() as u32;
  let pixel = frame
    .pixmap
    .pixel(x_px, y_px)
    .unwrap_or_else(|| panic!("pixel out of bounds at ({x_px},{y_px})"));
  (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha())
}

fn recv_until<T>(
  rx: &Receiver<WorkerToUi>,
  timeout: Duration,
  mut f: impl FnMut(WorkerToUi) -> Option<T>,
) -> T {
  let deadline = Instant::now() + timeout;
  loop {
    let now = Instant::now();
    let remaining = deadline
      .checked_duration_since(now)
      .unwrap_or(Duration::from_secs(0));
    assert!(
      remaining > Duration::from_secs(0),
      "timed out waiting for expected WorkerToUi message"
    );

    let msg = rx
      .recv_timeout(remaining)
      .unwrap_or_else(|err| panic!("timed out waiting for WorkerToUi message: {err}"));
    if let Some(value) = f(msg) {
      return value;
    }
  }
}

fn wait_for_frame_ready(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> fastrender::ui::messages::RenderedFrame {
  recv_until(rx, DEFAULT_TIMEOUT, |msg| match msg {
    WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => Some(frame),
    _ => None,
  })
}

fn wait_for_scroll_update(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> fastrender::scroll::ScrollState {
  recv_until(rx, DEFAULT_TIMEOUT, |msg| match msg {
    WorkerToUi::ScrollStateUpdated { tab_id: got, scroll } if got == tab_id => Some(scroll),
    _ => None,
  })
}

fn wait_for_frame_with_pixel(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  css_pos: (u32, u32),
  expected: (u8, u8, u8, u8),
) -> RenderedFrame {
  recv_until(rx, DEFAULT_TIMEOUT, |msg| match msg {
    WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => {
      (sample_rgba_at_css(&frame, css_pos.0, css_pos.1) == expected).then_some(frame)
    }
    _ => None,
  })
}

fn make_test_page() -> (tempfile::TempDir, String) {
  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #scroller {
            width: 120px;
            height: 60px;
            overflow-y: scroll;
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
  let url = format!("file://{}/index.html", dir.path().display());
  (dir, url)
}

fn make_test_page_scroller_far_down() -> (tempfile::TempDir, String) {
  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          .top-spacer { height: 500px; }
          #scroller {
            width: 120px;
            height: 60px;
            overflow-y: scroll;
            border: 1px solid black;
          }
          #scroller > .content {
            height: 400px;
            background: linear-gradient(#eee, #ccc);
          }
          .bottom-spacer { height: 2000px; }
        </style>
      </head>
      <body>
        <div class="top-spacer"></div>
        <div id="scroller"><div class="content"></div></div>
        <div class="bottom-spacer"></div>
      </body>
    </html>
  "#;

  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = format!("file://{}/index.html", dir.path().display());
  (dir, url)
}

#[test]
fn scroll_without_pointer_updates_viewport_scroll() {
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_test_page();

  let handle =
    spawn_ui_worker("fastr-ui-worker-scroll-without-pointer").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);
  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  let initial_scroll = frame.scroll_state.viewport;
  // `spawn_ui_worker` emits ScrollStateUpdated after FrameReady, so drain the initial navigation
  // update before we start asserting scroll behavior.
  let _ = wait_for_scroll_update(&ui_rx, tab_id);

  ui_tx
    .send(scroll_msg(tab_id, (0.0, 40.0), None))
    .expect("Scroll");

  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  let updated = wait_for_scroll_update(&ui_rx, tab_id);

  assert!(
    (updated.viewport.y - (initial_scroll.y + 40.0)).abs() < 1e-3,
    "expected viewport y scroll to increase by 40, was {:?} then {:?}",
    initial_scroll,
    updated.viewport
  );
  assert_eq!(
    frame.scroll_state, updated,
    "FrameReady.scroll_state should match ScrollStateUpdated"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn scroll_with_pointer_updates_element_scroll_offsets() {
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_test_page();

  let handle =
    spawn_ui_worker("fastr-ui-worker-scroll-with-pointer").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);
  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let _ = wait_for_frame_ready(&ui_rx, tab_id);
  let _ = wait_for_scroll_update(&ui_rx, tab_id);

  // Inside the #scroller element (it starts at the top of the page with margin: 0).
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 60.0), Some((10.0, 10.0))))
    .expect("Scroll");

  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  let updated = wait_for_scroll_update(&ui_rx, tab_id);

  assert!(
    updated.elements.len() >= 1,
    "expected at least one element scroll offset, got {:?}",
    updated.elements
  );
  assert!(
    updated.elements.values().any(|pt| pt.y > 0.0),
    "expected at least one element to scroll on y, got {:?}",
    updated.elements
  );
  assert_eq!(
    frame.scroll_state, updated,
    "FrameReady.scroll_state should match ScrollStateUpdated"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn scroll_with_pointer_after_viewport_scroll_targets_element() {
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_test_page_scroller_far_down();

  let (ui_tx, ui_rx, join) = spawn_ui_worker("fastr-ui-worker-scroll-with-pointer-after-viewport-scroll")
    .expect("spawn ui worker")
    .split();
  let tab_id = TabId(1);
  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let _ = wait_for_frame_ready(&ui_rx, tab_id);
  // Drain the initial navigation scroll update before asserting scroll behavior.
  let _ = wait_for_scroll_update(&ui_rx, tab_id);

  // Scroll the viewport so the #scroller element (positioned at y=500) is visible at the top of
  // the viewport.
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 500.0), None))
    .expect("Scroll viewport");
  let _ = wait_for_frame_ready(&ui_rx, tab_id);
  let after_viewport_scroll = wait_for_scroll_update(&ui_rx, tab_id);

  assert!(
    (after_viewport_scroll.viewport.y - 500.0).abs() < 2.0,
    "expected viewport y scroll to be ~500, got {:?}",
    after_viewport_scroll.viewport
  );

  // Wheel scroll at a small viewport-local coordinate near the top should target #scroller even
  // though the viewport is already scrolled. This only works if `pointer_css` is interpreted as
  // viewport-local and the worker adds `ScrollState.viewport` internally.
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 60.0), Some((10.0, 10.0))))
    .expect("Scroll scroller element");

  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  let updated = wait_for_scroll_update(&ui_rx, tab_id);

  assert!(
    (updated.viewport.y - after_viewport_scroll.viewport.y).abs() < 1e-3,
    "expected viewport scroll to remain unchanged when scrolling element, was {:?} then {:?}",
    after_viewport_scroll.viewport,
    updated.viewport
  );
  assert!(
    updated.elements.values().any(|pt| pt.y > 0.0),
    "expected element scroll to increase after wheel over #scroller, got {:?}",
    updated.elements
  );
  assert_eq!(
    frame.scroll_state, updated,
    "FrameReady.scroll_state should match ScrollStateUpdated"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn scroll_with_pointer_outside_scroller_scrolls_viewport() {
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_test_page();

  let (ui_tx, ui_rx, join) =
    spawn_ui_worker("fastr-ui-worker-scroll-with-pointer-outside-scroller").expect("spawn ui worker").split();
  let tab_id = TabId(1);
  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  let initial_scroll = frame.scroll_state.viewport;
  let _ = wait_for_scroll_update(&ui_rx, tab_id);

  // Outside the #scroller element (it is 120px wide; use x=150px).
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 40.0), Some((150.0, 10.0))))
    .expect("Scroll");

  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  let updated = wait_for_scroll_update(&ui_rx, tab_id);

  assert!(
    updated.elements.is_empty(),
    "expected no element scroll offsets when scrolling outside scroller, got {:?}",
    updated.elements
  );
  assert!(
    (updated.viewport.y - (initial_scroll.y + 40.0)).abs() < 1e-3,
    "expected viewport y scroll to increase by 40, was {:?} then {:?}",
    initial_scroll,
    updated.viewport
  );
  assert_eq!(
    frame.scroll_state, updated,
    "FrameReady.scroll_state should match ScrollStateUpdated"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn scroll_clamps_to_zero() {
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_test_page();

  let handle =
    spawn_ui_worker("fastr-ui-worker-scroll-clamp-zero").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);
  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let _ = wait_for_frame_ready(&ui_rx, tab_id);
  let _ = wait_for_scroll_update(&ui_rx, tab_id);

  // Ensure we're scrolled away from 0 so the clamp can be observed.
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 120.0), None))
    .expect("Scroll down");
  let _ = wait_for_frame_ready(&ui_rx, tab_id);
  let _ = wait_for_scroll_update(&ui_rx, tab_id);

  ui_tx
    .send(scroll_msg(tab_id, (0.0, -10_000.0), None))
    .expect("Scroll up");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  let updated = wait_for_scroll_update(&ui_rx, tab_id);

  assert!(
    updated.viewport.y.abs() < 1e-3,
    "expected viewport scroll to clamp to 0, got {:?}",
    updated.viewport
  );
  assert_eq!(
    frame.scroll_state, updated,
    "FrameReady.scroll_state should match ScrollStateUpdated"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn pointer_hit_testing_uses_element_scroll_offsets() {
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; background: white; }
          #scroller {
            width: 80px;
            height: 60px;
            overflow-y: scroll;
            border: 1px solid black;
            background: white;
          }
          #pad { padding-top: 120px; }
          #cb { position: absolute; left: -9999px; top: 0; }
          #lbl { display: block; }
          #box { width: 64px; height: 64px; background: rgb(255, 0, 0); }
          input[checked] + #lbl #box { background: rgb(0, 255, 0); }
        </style>
      </head>
      <body>
        <div id="scroller">
          <div id="pad">
            <input type="checkbox" id="cb">
            <label id="lbl" for="cb"><div id="box"></div></label>
          </div>
        </div>
      </body>
    </html>
  "#;
  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = format!("file://{}/index.html", dir.path().display());

  let handle = spawn_ui_worker("fastr-ui-worker-scroll-pointer-hit-test").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (160, 120), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let _ = wait_for_frame_ready(&ui_rx, tab_id);
  // `spawn_ui_worker` emits ScrollStateUpdated after FrameReady, so drain the initial navigation
  // scroll state update before asserting wheel scroll behavior.
  let _ = wait_for_scroll_update(&ui_rx, tab_id);

  // Scroll the nested scroll container so the red label box becomes visible at the top.
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 120.0), Some((10.0, 10.0))))
    .expect("Scroll");

  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  let updated = wait_for_scroll_update(&ui_rx, tab_id);
  assert!(
    !updated.elements.is_empty(),
    "expected element scroll offsets after pointer-based scroll, got {:?}",
    updated.elements
  );
  assert_eq!(
    sample_rgba_at_css(&frame, 20, 20),
    (255, 0, 0, 255),
    "expected scrolled box to be visible (red) before click"
  );

  // Click the visible box. Without applying scroll offsets to the fragment tree used for pointer
  // hit-testing, this click would target the pre-scroll coordinates and fail to toggle the
  // checkbox.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (20.0, 20.0),
      button: PointerButton::Primary,
    })
    .expect("PointerDown");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (20.0, 20.0),
      button: PointerButton::Primary,
    })
    .expect("PointerUp");

  let frame = wait_for_frame_with_pixel(&ui_rx, tab_id, (20, 20), (0, 255, 0, 255));
  assert_eq!(sample_rgba_at_css(&frame, 20, 20), (0, 255, 0, 255));

  drop(ui_tx);
  join.join().expect("join ui worker");
}
