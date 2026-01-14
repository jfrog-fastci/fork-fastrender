#![cfg(feature = "browser_ui")]

use super::support::{
  create_tab_msg, key_action, navigate_msg, pointer_down, pointer_up, viewport_changed_msg,
  DEFAULT_TIMEOUT,
};
use fastrender::interaction::KeyAction;
use fastrender::ui::messages::{
  NavigationReason, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::time::{Duration, Instant};
use tempfile::tempdir;

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

fn try_wait_for_frame_ready(
  rx: &fastrender::ui::WorkerToUiInbox,
  tab_id: TabId,
  timeout: Duration,
) -> Option<fastrender::ui::messages::RenderedFrame> {
  let deadline = Instant::now() + timeout;
  loop {
    let now = Instant::now();
    let remaining = deadline.checked_duration_since(now)?;
    match rx.recv_timeout(remaining) {
      Ok(msg) => match msg {
        WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => return Some(frame),
        _ => continue,
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return None,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
    }
  }
}

fn make_test_page() -> (tempfile::TempDir, String) {
  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; background: rgb(0,0,0); }

          /* The input comes first in the DOM so we can use adjacent sibling selectors, but it is
             positioned below the box so pointer clicks do not affect the sampled pixels. */
          #txt {
            position: absolute;
            left: 0;
            top: 80px;
            width: 140px;
            height: 24px;
          }

          #box {
            position: absolute;
            left: 0;
            top: 0;
            width: 64px;
            height: 64px;
            background: rgb(255,0,0);
          }

          input[value="abc"] + #box { background: rgb(0,0,255); }
          input[value="ab"] + #box { background: rgb(0,255,0); }

           /* Keep the background color reserved for input value assertions; use an outline to
              indicate focus-visible and sample pixels outside the box. */
          input:focus-visible + #box { outline: 4px solid rgb(255,255,0); }
         </style>
       </head>
       <body>
         <input id="txt" value="abc" />
        <div id="box"></div>
      </body>
    </html>
  "#;

  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = url::Url::from_file_path(dir.path().join("index.html"))
    .unwrap()
    .to_string();
  (dir, url)
}

fn make_autofocus_page() -> (tempfile::TempDir, String) {
  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; background: rgb(0,0,0); }

          /* The input comes first in the DOM so we can use adjacent sibling selectors, but it is
             positioned below the box so pointer clicks do not affect the sampled pixels. */
          #txt {
            position: absolute;
            left: 0;
            top: 80px;
            width: 140px;
            height: 24px;
          }

          #box {
            position: absolute;
            left: 0;
            top: 0;
            width: 64px;
            height: 64px;
            background: rgb(255,0,0);
          }

          input[value="abc"] + #box { background: rgb(0,0,255); }
          input[value="ab"] + #box { background: rgb(0,255,0); }

           /* Keep the background color reserved for input value assertions; use an outline to
              indicate focus-visible and sample pixels outside the box. */
          input:focus-visible + #box { outline: 4px solid rgb(255,255,0); }
         </style>
       </head>
       <body>
         <input id="txt" value="abc" autofocus />
        <div id="box"></div>
      </body>
    </html>
  "#;

  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = url::Url::from_file_path(dir.path().join("index.html"))
    .unwrap()
    .to_string();
  (dir, url)
}

fn pixel_rgba(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  assert!(x < pixmap.width(), "x out of bounds");
  assert!(y < pixmap.height(), "y out of bounds");
  let idx = (y * pixmap.width() + x) as usize * 4;
  let data = pixmap.data();
  let a = data[idx + 3];
  if a == 0 {
    return (0, 0, 0, 0);
  }
  // tiny-skia uses premultiplied alpha, so unpremultiply for stable comparisons.
  let r = ((data[idx] as u16 * 255) / a as u16) as u8;
  let g = ((data[idx + 1] as u16 * 255) / a as u16) as u8;
  let b = ((data[idx + 2] as u16 * 255) / a as u16) as u8;
  (r, g, b, a)
}

fn assert_pixel_rgb(pixmap: &tiny_skia::Pixmap, x: u32, y: u32, expected: (u8, u8, u8)) {
  let (r, g, b, a) = pixel_rgba(pixmap, x, y);
  assert_eq!(
    (r, g, b, a),
    (expected.0, expected.1, expected.2, 255),
    "unexpected pixel at ({x}, {y})"
  );
}

fn make_tab_traversal_page() -> (tempfile::TempDir, String) {
  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; background: rgb(0,0,0); }

          #a {
            position: absolute;
            left: 0;
            top: 30px;
            width: 80px;
            height: 20px;
          }

          #b {
            position: absolute;
            left: 0;
            top: 60px;
            width: 80px;
            height: 20px;
          }

          #status {
            position: absolute;
            left: 0;
            top: 0;
            width: 20px;
            height: 20px;
            background: rgb(0,0,0);
          }

           /* Focus should set a deterministic marker color. */
          #a:focus ~ #status { background: rgb(255,0,0); }
          #b:focus ~ #status { background: rgb(0,0,255); }

           /* Focus-visible should override focus when keyboard traversal is used. */
          #a:focus-visible ~ #status { background: rgb(255,0,255); }
          #b:focus-visible ~ #status { background: rgb(0,255,255); }
         </style>
       </head>
       <body>
         <input id="a" value="a" />
        <input id="b" value="b" />
        <div id="status"></div>
      </body>
    </html>
  "#;

  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = format!("file://{}/index.html", dir.path().display());
  (dir, url)
}

fn make_positive_tabindex_page() -> (tempfile::TempDir, String) {
  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; background: rgb(0,0,0); }

          #a { position: absolute; left: 0; top: 30px; width: 80px; height: 20px; }
          #b { position: absolute; left: 0; top: 60px; width: 80px; height: 20px; }
          #c { position: absolute; left: 0; top: 90px; width: 80px; height: 20px; }

          #status {
            position: absolute;
            left: 0;
            top: 0;
            width: 20px;
            height: 20px;
            background: rgb(0,0,0);
          }

          /* The status box color encodes the currently focused element. */
          #a:focus-visible ~ #status { background: rgb(255,0,0); }
          #b:focus-visible ~ #status { background: rgb(0,255,0); }
          #c:focus-visible ~ #status { background: rgb(0,0,255); }
        </style>
      </head>
      <body>
        <!-- DOM order: a, b, c -->
        <input id="a" value="a" tabindex="0" />
        <input id="b" value="b" tabindex="2" />
        <input id="c" value="c" tabindex="1" />
        <div id="status"></div>
      </body>
    </html>
  "#;

  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = url::Url::from_file_path(dir.path().join("index.html"))
    .unwrap()
    .to_string();
  (dir, url)
}

fn make_modal_focus_trap_page() -> (tempfile::TempDir, String) {
  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; background: rgb(0,0,0); }

           dialog {
             position: absolute;
             /* Override the UA dialog centering (`inset: 0; margin: auto`) so our sampled pixels
                deterministically hit the status box in the top-left corner. */
             inset: auto;
             margin: 0;
             left: 0;
             top: 0;
             width: 100px;
             height: 100px;
             padding: 0;
            border: none;
            background: rgb(0,0,0);
          }

          #in1 { position: absolute; left: 0; top: 30px; width: 80px; height: 20px; }
          #in2 { position: absolute; left: 0; top: 60px; width: 80px; height: 20px; }

          #status {
            position: absolute;
            left: 0;
            top: 0;
            width: 20px;
            height: 20px;
            background: rgb(0,0,0);
          }

          /* Focus-visible inside the dialog sets the status box. */
          #in1:focus-visible ~ #status { background: rgb(255,0,0); }
          #in2:focus-visible ~ #status { background: rgb(0,0,255); }

          /* Marker that turns green if focus ever escapes to the outside input. */
          #outside_marker {
            position: absolute;
            left: 110px;
            top: 0;
            width: 20px;
            height: 20px;
            background: rgb(0,0,0);
          }
          #outside {
            position: absolute;
            left: 0;
            top: 140px;
            width: 80px;
            height: 20px;
          }
          #outside:focus-visible + #outside_marker { background: rgb(0,255,0); }
        </style>
      </head>
      <body>
        <dialog id="dlg" data-fastr-open="modal">
          <input id="in1" value="a" />
          <input id="in2" value="b" />
          <div id="status"></div>
        </dialog>

        <input id="outside" value="outside" />
        <div id="outside_marker"></div>
      </body>
    </html>
  "#;

  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = url::Url::from_file_path(dir.path().join("index.html"))
    .unwrap()
    .to_string();
  (dir, url)
}

fn make_focus_attr_regression_page() -> (tempfile::TempDir, String) {
  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; background: rgb(0,0,0); }

          #txt {
            position: absolute;
            left: 0;
            top: 80px;
            width: 140px;
            height: 24px;
          }

          #marker {
            position: absolute;
            left: 0;
            top: 0;
            width: 64px;
            height: 64px;
            background: rgb(0,0,255);
          }

          /* Real focus should win. */
          #txt:focus + #marker { background: rgb(0,255,0); }

          /* This should NEVER match; the engine must not inject data-fastr-focus. */
          #txt[data-fastr-focus] + #marker { background: rgb(255,0,0); }
        </style>
      </head>
      <body>
        <input id="txt" value="x" />
        <div id="marker"></div>
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
fn backspace_edits_focused_input_and_repaints() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_test_page();

  let handle = spawn_ui_worker("fastr-ui-worker-keyboard-backspace").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);
  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (100, 120), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 0, 255));

  // Click the input to focus it.
  ui_tx
    .send(pointer_down(tab_id, (10.0, 90.0), PointerButton::Primary))
    .expect("PointerDown");
  ui_tx
    .send(pointer_up(tab_id, (10.0, 90.0), PointerButton::Primary))
    .expect("PointerUp");
  // The headless worker repaints both PointerDown and PointerUp; consume both frames so the
  // subsequent KeyAction assertion doesn't accidentally read a stale frame.
  let _ = wait_for_frame_ready(&ui_rx, tab_id);
  let _ = wait_for_frame_ready(&ui_rx, tab_id);

  // Real text editing is caret-based. The click above lands near the start of the input, so ensure
  // the caret is at the end before we press backspace (matching what the old "delete-at-end"
  // semantics were asserting).
  ui_tx.send(key_action(tab_id, KeyAction::End)).expect("End");
  let _ = wait_for_frame_ready(&ui_rx, tab_id);

  ui_tx
    .send(key_action(tab_id, KeyAction::Backspace))
    .expect("Backspace");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 255, 0));

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn delete_edits_focused_input_selection_and_repaints() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_test_page();

  let handle = spawn_ui_worker("fastr-ui-worker-keyboard-delete").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);
  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (100, 120), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 0, 255));

  // Click the input to focus it.
  ui_tx
    .send(pointer_down(tab_id, (10.0, 90.0), PointerButton::Primary))
    .expect("PointerDown");
  ui_tx
    .send(pointer_up(tab_id, (10.0, 90.0), PointerButton::Primary))
    .expect("PointerUp");
  // Consume PointerDown + PointerUp repaints.
  let _ = wait_for_frame_ready(&ui_rx, tab_id);
  let _ = wait_for_frame_ready(&ui_rx, tab_id);

  // SelectAll should update the worker's selection state and trigger a repaint so caret/selection
  // highlights can be rendered.
  ui_tx
    .send(UiToWorker::SelectAll { tab_id })
    .expect("SelectAll");
  let _ = wait_for_frame_ready(&ui_rx, tab_id);

  ui_tx
    .send(key_action(tab_id, KeyAction::Delete))
    .expect("Delete");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (255, 0, 0));

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn focus_does_not_inject_data_fastr_focus_attribute() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_focus_attr_regression_page();

  let handle =
    spawn_ui_worker("fastr-ui-worker-keyboard-focus-regression").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);
  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (100, 120), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  // Marker should start blue.
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 0, 255));

  // Click the input to focus it.
  ui_tx
    .send(pointer_down(tab_id, (10.0, 90.0), PointerButton::Primary))
    .expect("PointerDown");
  ui_tx
    .send(pointer_up(tab_id, (10.0, 90.0), PointerButton::Primary))
    .expect("PointerUp");
  // Consume PointerDown + PointerUp repaints.
  let _ = wait_for_frame_ready(&ui_rx, tab_id);
  let frame = wait_for_frame_ready(&ui_rx, tab_id);

  // Focus should make the marker green. If the renderer injected `data-fastr-focus`, the red rule
  // (declared later) would override.
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 255, 0));

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn clear_page_focus_blurs_focused_element_and_repaints() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_focus_attr_regression_page();

  let handle =
    spawn_ui_worker("fastr-ui-worker-clear-page-focus").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);
  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (100, 120), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  // Unfocused marker should start blue.
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 0, 255));

  // Click the input to focus it.
  ui_tx
    .send(pointer_down(tab_id, (10.0, 90.0), PointerButton::Primary))
    .expect("PointerDown");
  ui_tx
    .send(pointer_up(tab_id, (10.0, 90.0), PointerButton::Primary))
    .expect("PointerUp");
  // Consume PointerDown + PointerUp repaints.
  let _ = wait_for_frame_ready(&ui_rx, tab_id);
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 255, 0));

  ui_tx
    .send(UiToWorker::ClearPageFocus { tab_id })
    .expect("ClearPageFocus");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 0, 255));

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn key_action_sets_focus_visible() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_test_page();

  let handle = spawn_ui_worker("fastr-ui-worker-keyboard-focus-visible").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);
  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (100, 120), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  // Sample outside the box (right edge) to see the focus-visible outline.
  assert_pixel_rgb(&frame.pixmap, 66, 32, (0, 0, 0));

  ui_tx
    .send(pointer_down(tab_id, (10.0, 90.0), PointerButton::Primary))
    .expect("PointerDown");
  ui_tx
    .send(pointer_up(tab_id, (10.0, 90.0), PointerButton::Primary))
    .expect("PointerUp");
  // Consume PointerDown + PointerUp repaints.
  let _ = wait_for_frame_ready(&ui_rx, tab_id);
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 66, 32, (0, 0, 0));

  ui_tx
    // Use a key that is expected to keep focus on the currently focused input. `Tab` is likely
    // to become focus traversal once implemented.
    .send(key_action(tab_id, KeyAction::Backspace))
    .expect("Backspace");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 66, 32, (255, 255, 0));

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn autofocus_focuses_element_and_sets_focus_visible_on_load() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_autofocus_page();

  let handle = spawn_ui_worker("fastr-ui-worker-autofocus").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);
  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (100, 120), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  // Autofocus should apply before the first frame, and should set focus-visible.
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 0, 255));
  // Sample outside the box (right edge) to see the focus-visible outline.
  assert_pixel_rgb(&frame.pixmap, 66, 32, (255, 255, 0));

  // Autofocused inputs should have an initialized caret so text editing works immediately.
  ui_tx
    .send(key_action(tab_id, KeyAction::Backspace))
    .expect("Backspace");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 255, 0));

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn tab_order_honors_positive_tabindex_ordering() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_positive_tabindex_page();

  let handle = spawn_ui_worker("fastr-ui-worker-keyboard-tabindex-order").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);
  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (120, 120), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  // No focus initially.
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 0, 0));

  // Tab order should be: tabindex=1 (#c), then tabindex=2 (#b), then tabindex=0 (#a).
  ui_tx.send(key_action(tab_id, KeyAction::Tab)).expect("Tab");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 0, 255));

  ui_tx.send(key_action(tab_id, KeyAction::Tab)).expect("Tab");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 255, 0));

  ui_tx.send(key_action(tab_id, KeyAction::Tab)).expect("Tab");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (255, 0, 0));

  // Wrap back to the first positive tabindex element.
  ui_tx.send(key_action(tab_id, KeyAction::Tab)).expect("Tab");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 0, 255));

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn tab_focus_is_trapped_within_modal_dialog() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_modal_focus_trap_page();

  let handle =
    spawn_ui_worker("fastr-ui-worker-keyboard-modal-focus-trap").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);
  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (140, 180), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  // No focus initially.
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 0, 0));
  assert_pixel_rgb(&frame.pixmap, 120, 10, (0, 0, 0));

  // Tab should enter the modal and cycle between its focusable controls.
  ui_tx.send(key_action(tab_id, KeyAction::Tab)).expect("Tab");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (255, 0, 0));
  assert_pixel_rgb(&frame.pixmap, 120, 10, (0, 0, 0));

  ui_tx.send(key_action(tab_id, KeyAction::Tab)).expect("Tab");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 0, 255));
  assert_pixel_rgb(&frame.pixmap, 120, 10, (0, 0, 0));

  // Wrap within the modal (must not escape to the outside input).
  ui_tx.send(key_action(tab_id, KeyAction::Tab)).expect("Tab");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (255, 0, 0));
  assert_pixel_rgb(&frame.pixmap, 120, 10, (0, 0, 0));

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn tab_and_shift_tab_traverse_focus_and_wrap_in_ui_worker() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_tab_traversal_page();

  let handle = spawn_ui_worker("fastr-ui-worker-keyboard-tab-traversal").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);
  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (120, 120), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  // No focus initially, so the status box stays black.
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 0, 0));

  // Tab from no focus should focus the first focusable element and set focus-visible.
  ui_tx
    .send(UiToWorker::KeyAction {
      tab_id,
      key: KeyAction::Tab,
    })
    .expect("Tab");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (255, 0, 255));

  // Tab should advance to the next focusable element.
  ui_tx
    .send(UiToWorker::KeyAction {
      tab_id,
      key: KeyAction::Tab,
    })
    .expect("Tab");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 255, 255));

  // Tab should wrap back to the first focusable element.
  ui_tx
    .send(UiToWorker::KeyAction {
      tab_id,
      key: KeyAction::Tab,
    })
    .expect("Tab");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (255, 0, 255));

  // Click the background to clear focus so we can test Shift+Tab from "no focus".
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      // Click below the inputs. Note: form controls have UA padding/border, so their hit-test
      // bounds extend beyond the authored `width`/`height`; use a point far enough away that we
      // definitely don't hit them.
      pos_css: (110.0, 110.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("PointerDown");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (110.0, 110.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("PointerUp");
  let mut frame = wait_for_frame_ready(&ui_rx, tab_id);
  // PointerDown/PointerUp may trigger 1 or 2 repaints depending on whether PointerDown mutated any
  // DOM flags for the hit-tested element. If a second repaint arrives quickly, prefer it so we
  // assert against the final post-PointerUp state.
  if let Some(next) = try_wait_for_frame_ready(&ui_rx, tab_id, Duration::from_millis(250)) {
    frame = next;
  }
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 0, 0));

  // Shift+Tab from no focus should focus the last focusable element and set focus-visible.
  ui_tx
    .send(UiToWorker::KeyAction {
      tab_id,
      key: KeyAction::ShiftTab,
    })
    .expect("ShiftTab");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 255, 255));

  // Shift+Tab should traverse backwards.
  ui_tx
    .send(UiToWorker::KeyAction {
      tab_id,
      key: KeyAction::ShiftTab,
    })
    .expect("ShiftTab");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (255, 0, 255));

  drop(ui_tx);
  join.join().expect("join ui worker");
}
