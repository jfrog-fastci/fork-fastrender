#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{
  NavigationReason, PointerButton, RenderedFrame, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::worker_loop::spawn_ui_worker;
use fastrender::tree::box_tree::SelectItem;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use tempfile::tempdir;

// These tests spin up real UI worker threads that create renderers and rasterize frames.
// When the test binary runs with many threads (default), CPU contention can make the first render
// take longer than a couple seconds on busy CI hosts. Keep the timeout generous to avoid flakiness
// while still failing quickly on genuine deadlocks.
const TIMEOUT: Duration = Duration::from_secs(10);

fn sample_rgba_at_css(frame: &RenderedFrame, x_css: u32, y_css: u32) -> (u8, u8, u8, u8) {
  let x_px = ((x_css as f32) * frame.dpr).round() as u32;
  let y_px = ((y_css as f32) * frame.dpr).round() as u32;
  let pixel = frame
    .pixmap
    .pixel(x_px, y_px)
    .unwrap_or_else(|| panic!("pixel out of bounds at ({x_px},{y_px})"));
  (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha())
}

fn recv_until_frame(rx: &Receiver<WorkerToUi>, tab_id: TabId, deadline: Instant) -> RenderedFrame {
  loop {
    let now = Instant::now();
    if now >= deadline {
      panic!("timed out waiting for FrameReady");
    }
    let remaining = deadline.saturating_duration_since(now);
    match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(msg) => match msg {
        WorkerToUi::FrameReady { tab_id: msg_tab, frame } if msg_tab == tab_id => return frame,
        _ => {}
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
        panic!("worker channel disconnected while waiting for FrameReady");
      }
    }
  }
}

fn recv_until_pixel(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  css_pos: (u32, u32),
  expected: (u8, u8, u8, u8),
  deadline: Instant,
) -> RenderedFrame {
  loop {
    let frame = recv_until_frame(rx, tab_id, deadline);
    let rgba = sample_rgba_at_css(&frame, css_pos.0, css_pos.1);
    if rgba == expected {
      return frame;
    }
  }
}

#[test]
fn label_click_toggles_checkbox_and_repaints() {
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let html_path = dir.path().join("page.html");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #cb { position: absolute; left: -9999px; top: 0; }
          #lbl { display: block; position: absolute; left: 0; top: 0; }
          #box { width: 64px; height: 64px; background: rgb(255, 0, 0); }
          input[checked] + #lbl #box { background: rgb(0, 255, 0); }
        </style>
      </head>
      <body>
        <input type="checkbox" id="cb">
        <label id="lbl" for="cb"><div id="box"></div></label>
      </body>
    </html>
  "#;
  std::fs::write(&html_path, html).expect("write html");
  let file_url = format!("file://{}", html_path.display());

  let handle = spawn_ui_worker("fastr-ui-worker-interaction-a").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();
  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: Default::default(),
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (128, 128),
      dpr: 1.0,
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: file_url,
      reason: NavigationReason::TypedUrl,
    })
    .unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let frame = recv_until_frame(&ui_rx, tab_id, deadline);
  assert_eq!(sample_rgba_at_css(&frame, 10, 10), (255, 0, 0, 255));

  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let frame = recv_until_pixel(
    &ui_rx,
    tab_id,
    (10, 10),
    (0, 255, 0, 255),
    deadline,
  );
  assert_eq!(sample_rgba_at_css(&frame, 10, 10), (0, 255, 0, 255));

  drop(ui_tx);
  join.join().unwrap();
}

#[test]
fn text_input_updates_focused_input_value_and_repaints() {
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let html_path = dir.path().join("page.html");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 64px; height: 64px; background: rgb(255, 0, 0); }
          #txt { position: absolute; top: 80px; left: 0; width: 100px; height: 20px; }
          input[value="abc"] + #box { background: rgb(0, 0, 255); }
        </style>
      </head>
      <body>
        <input id="txt" value="">
        <div id="box"></div>
      </body>
    </html>
  "#;
  std::fs::write(&html_path, html).expect("write html");
  let file_url = format!("file://{}", html_path.display());

  let handle = spawn_ui_worker("fastr-ui-worker-interaction-b").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();
  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: Default::default(),
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (160, 160),
      dpr: 1.0,
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: file_url,
      reason: NavigationReason::TypedUrl,
    })
    .unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let frame = recv_until_frame(&ui_rx, tab_id, deadline);
  assert_eq!(sample_rgba_at_css(&frame, 10, 10), (255, 0, 0, 255));

  // Focus input.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 90.0),
      button: PointerButton::Primary,
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 90.0),
      button: PointerButton::Primary,
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::TextInput {
      tab_id,
      text: "abc".to_string(),
    })
    .unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let frame = recv_until_pixel(
    &ui_rx,
    tab_id,
    (10, 10),
    (0, 0, 255, 255),
    deadline,
  );
  assert_eq!(sample_rgba_at_css(&frame, 10, 10), (0, 0, 255, 255));

  drop(ui_tx);
  join.join().unwrap();
}

#[test]
fn link_click_triggers_navigation_to_resolved_url() {
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let page1_path = dir.path().join("page1.html");
  let page2_path = dir.path().join("page2.html");

  let page1 = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #link { display: block; width: 100px; height: 40px; background: rgb(255, 0, 0); }
        </style>
      </head>
      <body>
        <a href="page2.html" id="link">Go</a>
      </body>
    </html>
  "#;
  let page2 = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; background: rgb(0, 255, 0); }
        </style>
      </head>
      <body>Second</body>
    </html>
  "#;

  std::fs::write(&page1_path, page1).expect("write page1");
  std::fs::write(&page2_path, page2).expect("write page2");

  let page1_url = format!("file://{}", page1_path.display());
  let page2_url = format!("file://{}", page2_path.display());

  let handle = spawn_ui_worker("fastr-ui-worker-interaction-c").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();
  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: Default::default(),
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (200, 120),
      dpr: 1.0,
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: page1_url,
      reason: NavigationReason::TypedUrl,
    })
    .unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let _frame = recv_until_frame(&ui_rx, tab_id, deadline);

  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let mut saw_started = false;
  let mut saw_committed = false;
  let mut saw_frame = false;

  while Instant::now() < deadline {
    match ui_rx.recv_timeout(Duration::from_millis(200)) {
      Ok(msg) => match msg {
        WorkerToUi::NavigationStarted { tab_id: msg_tab, url } if msg_tab == tab_id => {
          if url == page2_url {
            saw_started = true;
          }
        }
        WorkerToUi::NavigationCommitted { tab_id: msg_tab, url, .. } if msg_tab == tab_id => {
          if url == page2_url {
            saw_committed = true;
          }
        }
        WorkerToUi::FrameReady { tab_id: msg_tab, .. } if msg_tab == tab_id => {
          if saw_committed {
            saw_frame = true;
            break;
          }
        }
        _ => {}
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  assert!(saw_started, "expected NavigationStarted for page2");
  assert!(saw_committed, "expected NavigationCommitted for page2");
  assert!(saw_frame, "expected FrameReady after navigation committed");

  drop(ui_tx);
  join.join().unwrap();
}

#[test]
fn select_dropdown_click_emits_open_select_dropdown_message() {
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let html_path = dir.path().join("page.html");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #sel { position: absolute; left: 0; top: 0; width: 120px; height: 30px; }
        </style>
      </head>
      <body>
        <select id="sel">
          <option>One</option>
          <option selected>Two</option>
          <option>Three</option>
        </select>
      </body>
    </html>
  "#;
  std::fs::write(&html_path, html).expect("write html");
  let file_url = format!("file://{}", html_path.display());

  let (ui_tx, ui_rx, join) = spawn_ui_worker("fastr-ui-worker-interaction-select")
    .expect("spawn ui worker")
    .split();
  let tab_id = TabId::new();
  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: Default::default(),
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (200, 80),
      dpr: 1.0,
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: file_url,
      reason: NavigationReason::TypedUrl,
    })
    .unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let _frame = recv_until_frame(&ui_rx, tab_id, deadline);
  while ui_rx.try_recv().is_ok() {}

  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let mut received = None;
  while Instant::now() < deadline {
    match ui_rx.recv_timeout(Duration::from_millis(200)) {
      Ok(msg) => match msg {
        WorkerToUi::OpenSelectDropdown {
          tab_id: msg_tab,
          select_node_id,
          control,
        } if msg_tab == tab_id => {
          received = Some((select_node_id, control));
          break;
        }
        _ => {}
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  let (select_node_id, control) = received.expect("expected OpenSelectDropdown message");
  assert!(select_node_id > 0, "expected non-zero select_node_id");
  assert!(!control.multiple, "expected dropdown select to be single-select");
  assert_eq!(control.size, 1);
  assert_eq!(control.items.len(), 3);
  assert_eq!(control.selected, vec![1]);

  let labels: Vec<String> = control
    .items
    .iter()
    .filter_map(|item| match item {
      SelectItem::Option { label, .. } => Some(label.clone()),
      _ => None,
    })
    .collect();
  assert_eq!(labels, vec!["One", "Two", "Three"]);

  drop(ui_tx);
  join.join().unwrap();
}
