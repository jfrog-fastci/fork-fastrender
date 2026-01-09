#![cfg(feature = "browser_ui")]

use fastrender::interaction::KeyAction;
use super::support::{create_tab_msg, navigate_msg, viewport_changed_msg};
use fastrender::ui::messages::{NavigationReason, PointerButton, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::worker_loop::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use tempfile::tempdir;

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
  recv_until(rx, Duration::from_secs(10), |msg| match msg {
    WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => Some(frame),
    _ => None,
  })
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
          input[data-fastr-focus-visible="true"] + #box { outline: 4px solid rgb(255,255,0); }
        </style>
      </head>
      <body>
        <input id="txt" value="abc" />
        <div id="box"></div>
      </body>
    </html>
  "#;

  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = format!("file://{}/index.html", dir.path().display());
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

#[test]
fn backspace_edits_focused_input_and_repaints() {
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_test_page();

  let (ui_tx, ui_rx, join) = spawn_ui_worker("fastr-ui-worker-keyboard-backspace")
    .expect("spawn ui worker")
    .split();
  let tab_id = TabId(1);
  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("CreateTab");
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
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 90.0),
      button: PointerButton::Primary,
    })
    .expect("PointerDown");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 90.0),
      button: PointerButton::Primary,
    })
    .expect("PointerUp");
  // The headless worker repaints both PointerDown and PointerUp; consume both frames so the
  // subsequent KeyAction assertion doesn't accidentally read a stale frame.
  let _ = wait_for_frame_ready(&ui_rx, tab_id);
  let _ = wait_for_frame_ready(&ui_rx, tab_id);

  ui_tx
    .send(UiToWorker::KeyAction {
      tab_id,
      key: KeyAction::Backspace,
    })
    .expect("Backspace");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 10, 10, (0, 255, 0));

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn key_action_sets_focus_visible() {
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_test_page();

  let (ui_tx, ui_rx, join) = spawn_ui_worker("fastr-ui-worker-keyboard-focus-visible")
    .expect("spawn ui worker")
    .split();
  let tab_id = TabId(1);
  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("CreateTab");
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
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 90.0),
      button: PointerButton::Primary,
    })
    .expect("PointerDown");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 90.0),
      button: PointerButton::Primary,
    })
    .expect("PointerUp");
  // Consume PointerDown + PointerUp repaints.
  let _ = wait_for_frame_ready(&ui_rx, tab_id);
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 66, 32, (0, 0, 0));

  ui_tx
    .send(UiToWorker::KeyAction {
      tab_id,
      key: KeyAction::Tab,
    })
    .expect("Tab");
  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  assert_pixel_rgb(&frame.pixmap, 66, 32, (255, 255, 0));

  drop(ui_tx);
  join.join().expect("join ui worker");
}
