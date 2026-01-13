#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{NavigationReason, PointerButton, RenderedFrame, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::{Duration, Instant};
use tempfile::tempdir;

use super::support::{
  create_tab_msg, navigate_msg, pointer_down, pointer_up, scroll_msg, scroll_to_msg,
  viewport_changed_msg, DEFAULT_TIMEOUT,
};

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
  rx: &fastrender::ui::WorkerToUiInbox,
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

fn wait_for_frame_ready(
  rx: &fastrender::ui::WorkerToUiInbox,
  tab_id: TabId,
) -> fastrender::ui::messages::RenderedFrame {
  recv_until(rx, DEFAULT_TIMEOUT, |msg| match msg {
    WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => Some(frame),
    _ => None,
  })
}

fn wait_for_frame_with_pixel(
  rx: &fastrender::ui::WorkerToUiInbox,
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
  let url = url::Url::from_file_path(dir.path().join("index.html"))
    .unwrap()
    .to_string();
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
  let url = url::Url::from_file_path(dir.path().join("index.html"))
    .unwrap()
    .to_string();
  (dir, url)
}

fn make_test_page_wide() -> (tempfile::TempDir, String) {
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
          /* Force horizontal overflow so we can scroll `ScrollState.viewport.x` away from 0. */
          .wide { width: 1000px; height: 1px; }
          .spacer { height: 2000px; }
        </style>
      </head>
      <body>
        <div id="scroller"><div class="content"></div></div>
        <div class="wide"></div>
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
fn scroll_during_initial_navigation_is_applied_to_first_frame() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_test_page();

  let handle =
    spawn_ui_worker("fastr-ui-worker-scroll-initial-navigation").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  // Start the navigation immediately via `CreateTab` so we can send scroll input before the worker
  // has produced the first frame.
  ui_tx
    .send(create_tab_msg(tab_id, Some(url)))
    .expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 120.0), None))
    .expect("Scroll before first frame");

  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  let scroll = frame.scroll_state.clone();

  assert!(
    (scroll.viewport.y - 120.0).abs() < 1e-3,
    "expected initial frame to reflect scroll input before first paint (got {:?})",
    scroll.viewport
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn scroll_without_pointer_updates_viewport_scroll() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_test_page();

  let handle = spawn_ui_worker("fastr-ui-worker-scroll-without-pointer").expect("spawn ui worker");
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

  ui_tx
    .send(scroll_msg(tab_id, (0.0, 40.0), None))
    .expect("Scroll");

  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  let updated = frame.scroll_state.clone();

  assert!(
    (updated.viewport.y - (initial_scroll.y + 40.0)).abs() < 1e-3,
    "expected viewport y scroll to increase by 40, was {:?} then {:?}",
    initial_scroll,
    updated.viewport
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn scroll_with_pointer_updates_element_scroll_offsets() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_test_page();

  let handle = spawn_ui_worker("fastr-ui-worker-scroll-with-pointer").expect("spawn ui worker");
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

  // Inside the #scroller element (it starts at the top of the page with margin: 0).
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 60.0), Some((10.0, 10.0))))
    .expect("Scroll");

  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  let updated = frame.scroll_state.clone();

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

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn scroll_with_pointer_after_viewport_scroll_targets_element() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_test_page_scroller_far_down();

  let (ui_tx, ui_rx, join) =
    spawn_ui_worker("fastr-ui-worker-scroll-with-pointer-after-viewport-scroll")
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

  // Scroll the viewport so the #scroller element (positioned at y=500) is visible at the top of
  // the viewport.
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 500.0), None))
    .expect("Scroll viewport");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  let after_viewport_scroll = frame.scroll_state.clone();

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
  let updated = frame.scroll_state.clone();

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

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn scroll_with_negative_pointer_is_treated_as_viewport_scroll() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_test_page_wide();

  let (ui_tx, ui_rx, join) = spawn_ui_worker("fastr-ui-worker-scroll-negative-pointer")
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
  // Drain any follow-up navigation messages so subsequent waits only observe the scroll actions
  // below. The worker does not guarantee a `ScrollStateUpdated` after navigation.
  let _ = super::support::drain_for(&ui_rx, Duration::from_millis(200));

  // Move the viewport scroll away from the origin so a misbehaving `pointer_css: (-1,-1)` would
  // translate to an in-page point (scroll_x-1, scroll_y-1) and hit-test into the nested scroller.
  ui_tx
    .send(scroll_to_msg(tab_id, (11.0, 11.0)))
    .expect("ScrollTo");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  let after_scroll_to = frame.scroll_state.clone();
  assert!(
    after_scroll_to.viewport.x > 0.0 && after_scroll_to.viewport.y > 0.0,
    "expected ScrollTo to move viewport away from origin, got {:?}",
    after_scroll_to.viewport
  );
  assert!(
    (after_scroll_to.viewport.x - 11.0).abs() < 1e-3
      && (after_scroll_to.viewport.y - 11.0).abs() < 1e-3,
    "expected ScrollTo to set viewport scroll to (11,11), got {:?}",
    after_scroll_to.viewport
  );
  assert!(
    after_scroll_to.elements.is_empty(),
    "expected no element scroll offsets after ScrollTo, got {:?}",
    after_scroll_to.elements
  );

  ui_tx
    .send(scroll_msg(tab_id, (0.0, 40.0), Some((-1.0_f32, -1.0_f32))))
    .expect("Scroll with negative pointer");

  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  let updated = frame.scroll_state.clone();

  assert!(
    updated.elements.is_empty(),
    "expected negative pointer scroll to be treated as viewport scroll (got element offsets {:?})",
    updated.elements
  );
  assert!(
    updated.viewport.y > after_scroll_to.viewport.y,
    "expected viewport y scroll to increase after negative pointer scroll, was {:?} then {:?}",
    after_scroll_to.viewport,
    updated.viewport
  );
  assert!(
    (updated.viewport.y - (after_scroll_to.viewport.y + 40.0)).abs() < 1e-3,
    "expected negative pointer scroll to apply delta to viewport scroll (expected y≈{} got {})",
    after_scroll_to.viewport.y + 40.0,
    updated.viewport.y
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn scroll_with_pointer_outside_scroller_scrolls_viewport() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_test_page();

  let (ui_tx, ui_rx, join) =
    spawn_ui_worker("fastr-ui-worker-scroll-with-pointer-outside-scroller")
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

  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  let initial_scroll = frame.scroll_state.viewport;

  // Outside the #scroller element (it is 120px wide; use x=150px).
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 40.0), Some((150.0, 10.0))))
    .expect("Scroll");

  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  let updated = frame.scroll_state.clone();

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

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn scroll_clamps_to_zero() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_test_page();

  let handle = spawn_ui_worker("fastr-ui-worker-scroll-clamp-zero").expect("spawn ui worker");
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

  // Ensure we're scrolled away from 0 so the clamp can be observed.
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 120.0), None))
    .expect("Scroll down");
  let _ = wait_for_frame_ready(&ui_rx, tab_id);

  ui_tx
    .send(scroll_msg(tab_id, (0.0, -10_000.0), None))
    .expect("Scroll up");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  let updated = frame.scroll_state.clone();

  assert!(
    updated.viewport.y.abs() < 1e-3,
    "expected viewport scroll to clamp to 0, got {:?}",
    updated.viewport
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn pointer_hit_testing_uses_element_scroll_offsets() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
  let url = url::Url::from_file_path(dir.path().join("index.html"))
    .unwrap()
    .to_string();

  let handle = spawn_ui_worker("fastr-ui-worker-scroll-pointer-hit-test").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (160, 120), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let _ = wait_for_frame_ready(&ui_rx, tab_id);

  // Scroll the nested scroll container so the red label box becomes visible at the top.
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 120.0), Some((10.0, 10.0))))
    .expect("Scroll");

  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  let updated = frame.scroll_state.clone();
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
    .send(pointer_down(tab_id, (20.0, 20.0), PointerButton::Primary))
    .expect("PointerDown");
  ui_tx
    .send(pointer_up(tab_id, (20.0, 20.0), PointerButton::Primary))
    .expect("PointerUp");

  let frame = wait_for_frame_with_pixel(&ui_rx, tab_id, (20, 20), (0, 255, 0, 255));
  assert_eq!(sample_rgba_at_css(&frame, 20, 20), (0, 255, 0, 255));

  drop(ui_tx);
  join.join().expect("join ui worker");
}
