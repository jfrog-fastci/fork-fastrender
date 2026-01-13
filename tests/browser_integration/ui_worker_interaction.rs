#![cfg(feature = "browser_ui")]

use fastrender::tree::box_tree::SelectItem;
use fastrender::ui::messages::{NavigationReason, PointerButton, RenderedFrame, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::{Duration, Instant};
use tempfile::tempdir;

use super::support::{
  create_tab_msg, navigate_msg, pointer_down, pointer_up, scroll_at_pointer, text_input,
  viewport_changed_msg, DEFAULT_TIMEOUT,
};

// These tests spin up real UI worker threads that create renderers and rasterize frames.
// When the test binary runs with many threads (default), CPU contention can make the first render
// take longer than a couple seconds on busy CI hosts. Keep the timeout generous to avoid flakiness
// while still failing quickly on genuine deadlocks.
const TIMEOUT: Duration = DEFAULT_TIMEOUT;
// The legacy UI worker builds its own renderer instance per tab. In debug builds, the first
// navigation can be dominated by one-time initialization (font DB, caches), so allow more time for
// this select-specific smoke test.
const SELECT_DROPDOWN_TIMEOUT: Duration = Duration::from_secs(120);

fn sample_rgba_at_css(frame: &RenderedFrame, x_css: u32, y_css: u32) -> (u8, u8, u8, u8) {
  let x_px = ((x_css as f32) * frame.dpr).round() as u32;
  let y_px = ((y_css as f32) * frame.dpr).round() as u32;
  let pixel = frame
    .pixmap
    .pixel(x_px, y_px)
    .unwrap_or_else(|| panic!("pixel out of bounds at ({x_px},{y_px})"));
  (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha())
}

fn recv_until_frame(
  rx: &fastrender::ui::WorkerToUiInbox,
  tab_id: TabId,
  wait_for_navigation_committed_url: Option<&str>,
  deadline: Instant,
) -> RenderedFrame {
  let mut can_accept_frame = wait_for_navigation_committed_url.is_none();
  let mut last_msgs: std::collections::VecDeque<String> =
    std::collections::VecDeque::with_capacity(32);
  loop {
    let now = Instant::now();
    if now >= deadline {
      let mut dump = String::new();
      if !last_msgs.is_empty() {
        dump.push_str("\nlast worker messages:\n");
        for line in last_msgs {
          dump.push_str("  ");
          dump.push_str(&line);
          dump.push('\n');
        }
      }
      panic!("timed out waiting for FrameReady{dump}");
    }
    let remaining = deadline.saturating_duration_since(now);
    match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(msg) => {
        if last_msgs.len() == last_msgs.capacity() {
          last_msgs.pop_front();
        }
        last_msgs.push_back(format!("{msg:?}"));
        match msg {
          WorkerToUi::NavigationCommitted {
            tab_id: msg_tab,
            url,
            ..
          } if msg_tab == tab_id => {
            if wait_for_navigation_committed_url.map_or(false, |expected| expected == url) {
              can_accept_frame = true;
            }
          }
          WorkerToUi::NavigationFailed {
            tab_id: msg_tab,
            url,
            error,
            ..
          } if msg_tab == tab_id => {
            panic!("navigation failed for {url}: {error}");
          }
          WorkerToUi::FrameReady {
            tab_id: msg_tab,
            frame,
          } if msg_tab == tab_id => {
            if can_accept_frame {
              return frame;
            }
          }
          _ => {}
        }
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
        panic!("worker channel disconnected while waiting for FrameReady");
      }
    }
  }
}

fn recv_until_pixel(
  rx: &fastrender::ui::WorkerToUiInbox,
  tab_id: TabId,
  css_pos: (u32, u32),
  expected: (u8, u8, u8, u8),
  deadline: Instant,
) -> RenderedFrame {
  loop {
    let frame = recv_until_frame(rx, tab_id, None, deadline);
    let rgba = sample_rgba_at_css(&frame, css_pos.0, css_pos.1);
    if rgba == expected {
      return frame;
    }
  }
}

#[test]
fn label_click_toggles_checkbox_and_repaints() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
  let file_url = url::Url::from_file_path(&html_path).unwrap().to_string();

  let handle = spawn_ui_worker("fastr-ui-worker-interaction-a").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();
  ui_tx.send(create_tab_msg(tab_id, None)).unwrap();
  ui_tx
    .send(viewport_changed_msg(tab_id, (128, 128), 1.0))
    .unwrap();
  ui_tx
    .send(navigate_msg(
      tab_id,
      file_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let frame = recv_until_frame(&ui_rx, tab_id, Some(file_url.as_str()), deadline);
  assert_eq!(sample_rgba_at_css(&frame, 10, 10), (255, 0, 0, 255));

  ui_tx
    .send(pointer_down(tab_id, (10.0, 10.0), PointerButton::Primary))
    .unwrap();
  ui_tx
    .send(pointer_up(tab_id, (10.0, 10.0), PointerButton::Primary))
    .unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let frame = recv_until_pixel(&ui_rx, tab_id, (10, 10), (0, 255, 0, 255), deadline);
  assert_eq!(sample_rgba_at_css(&frame, 10, 10), (0, 255, 0, 255));

  drop(ui_tx);
  join.join().unwrap();
}

#[test]
fn text_input_updates_focused_input_value_and_repaints() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
  let file_url = url::Url::from_file_path(&html_path).unwrap().to_string();

  let handle = spawn_ui_worker("fastr-ui-worker-interaction-b").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();
  ui_tx.send(create_tab_msg(tab_id, None)).unwrap();
  ui_tx
    .send(viewport_changed_msg(tab_id, (160, 160), 1.0))
    .unwrap();
  ui_tx
    .send(navigate_msg(
      tab_id,
      file_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let frame = recv_until_frame(&ui_rx, tab_id, Some(file_url.as_str()), deadline);
  assert_eq!(sample_rgba_at_css(&frame, 10, 10), (255, 0, 0, 255));

  // Focus input.
  ui_tx
    .send(pointer_down(tab_id, (10.0, 90.0), PointerButton::Primary))
    .unwrap();
  ui_tx
    .send(pointer_up(tab_id, (10.0, 90.0), PointerButton::Primary))
    .unwrap();
  ui_tx.send(text_input(tab_id, "abc")).unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let frame = recv_until_pixel(&ui_rx, tab_id, (10, 10), (0, 0, 255, 255), deadline);
  assert_eq!(sample_rgba_at_css(&frame, 10, 10), (0, 0, 255, 255));

  drop(ui_tx);
  join.join().unwrap();
}

#[test]
fn link_click_triggers_navigation_to_resolved_url() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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

  let page1_url = url::Url::from_file_path(&page1_path).unwrap().to_string();
  let page2_url = url::Url::from_file_path(&page2_path).unwrap().to_string();

  let handle = spawn_ui_worker("fastr-ui-worker-interaction-c").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();
  ui_tx.send(create_tab_msg(tab_id, None)).unwrap();
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 120), 1.0))
    .unwrap();
  ui_tx
    .send(navigate_msg(
      tab_id,
      page1_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let _frame = recv_until_frame(&ui_rx, tab_id, Some(page1_url.as_str()), deadline);

  ui_tx
    .send(pointer_down(tab_id, (10.0, 10.0), PointerButton::Primary))
    .unwrap();
  ui_tx
    .send(pointer_up(tab_id, (10.0, 10.0), PointerButton::Primary))
    .unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let mut saw_started = false;
  let mut saw_committed = false;
  let mut saw_frame = false;

  while Instant::now() < deadline {
    match ui_rx.recv_timeout(Duration::from_millis(200)) {
      Ok(msg) => match msg {
        WorkerToUi::NavigationStarted {
          tab_id: msg_tab,
          url,
        } if msg_tab == tab_id => {
          if url == page2_url {
            saw_started = true;
          }
        }
        WorkerToUi::NavigationCommitted {
          tab_id: msg_tab,
          url,
          ..
        } if msg_tab == tab_id => {
          if url == page2_url {
            saw_committed = true;
          }
        }
        WorkerToUi::FrameReady {
          tab_id: msg_tab, ..
        } if msg_tab == tab_id => {
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
fn element_scroll_then_click_link_uses_scrolled_hit_testing() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let page1_path = dir.path().join("page1.html");
  let page2_path = dir.path().join("page2.html");

  // The link starts below the fold inside the scroller; after scrolling the element, it should be
  // clickable at the same viewport coordinate where it is painted.
  let page1 = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #scroller { width: 200px; height: 100px; overflow: auto; background: rgb(240, 240, 240); }
          #spacer { height: 120px; }
          #link { display: block; height: 40px; background: rgb(255, 0, 0); }
        </style>
      </head>
      <body>
        <div id="scroller">
          <div id="spacer"></div>
          <a href="page2.html" id="link">Go</a>
        </div>
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

  let page1_url = url::Url::from_file_path(&page1_path).unwrap().to_string();
  let page2_url = url::Url::from_file_path(&page2_path).unwrap().to_string();

  let handle =
    spawn_ui_worker("fastr-ui-worker-interaction-element-scroll").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();
  ui_tx.send(create_tab_msg(tab_id, None)).unwrap();
  ui_tx
    .send(viewport_changed_msg(tab_id, (240, 160), 1.0))
    .unwrap();
  ui_tx
    .send(navigate_msg(
      tab_id,
      page1_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let _frame = recv_until_frame(&ui_rx, tab_id, Some(page1_url.as_str()), deadline);
  while ui_rx.try_recv().is_ok() {}

  ui_tx
    .send(scroll_at_pointer(tab_id, (0.0, 1000.0), (10.0, 10.0)))
    .unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let frame = recv_until_frame(&ui_rx, tab_id, None, deadline);
  assert!(
    frame.scroll_state.viewport.y.abs() < 0.5,
    "expected viewport scroll to remain 0 when scrolling element; got {:?}",
    frame.scroll_state.viewport
  );
  assert!(
    frame
      .scroll_state
      .elements
      .values()
      .any(|offset| offset.y > 0.0),
    "expected element scroll offset after scroll_at_pointer; got {:?}",
    frame.scroll_state
  );

  // Click where the link is painted after scrolling.
  ui_tx
    .send(pointer_down(tab_id, (10.0, 60.0), PointerButton::Primary))
    .unwrap();
  ui_tx
    .send(pointer_up(tab_id, (10.0, 60.0), PointerButton::Primary))
    .unwrap();

  let deadline = Instant::now() + TIMEOUT;
  let mut saw_started = false;
  let mut saw_committed = false;
  let mut saw_frame = false;

  while Instant::now() < deadline {
    match ui_rx.recv_timeout(Duration::from_millis(200)) {
      Ok(msg) => match msg {
        WorkerToUi::NavigationStarted {
          tab_id: msg_tab,
          url,
        } if msg_tab == tab_id => {
          if url == page2_url {
            saw_started = true;
          }
        }
        WorkerToUi::NavigationCommitted {
          tab_id: msg_tab,
          url,
          ..
        } if msg_tab == tab_id => {
          if url == page2_url {
            saw_committed = true;
          }
        }
        WorkerToUi::FrameReady {
          tab_id: msg_tab, ..
        } if msg_tab == tab_id => {
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

  assert!(
    saw_started,
    "expected NavigationStarted for page2 after scrolled click"
  );
  assert!(
    saw_committed,
    "expected NavigationCommitted for page2 after scrolled click"
  );
  assert!(saw_frame, "expected FrameReady after navigation committed");

  drop(ui_tx);
  join.join().unwrap();
}

#[test]
fn select_dropdown_click_emits_select_dropdown_opened_message() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
  let file_url = url::Url::from_file_path(&html_path).unwrap().to_string();

  let handle = spawn_ui_worker("fastr-ui-worker-interaction-select").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();
  ui_tx.send(create_tab_msg(tab_id, None)).unwrap();
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 80), 1.0))
    .unwrap();
  ui_tx
    .send(navigate_msg(
      tab_id,
      file_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  let deadline = Instant::now() + SELECT_DROPDOWN_TIMEOUT;
  let _frame = recv_until_frame(&ui_rx, tab_id, Some(file_url.as_str()), deadline);
  while ui_rx.try_recv().is_ok() {}

  ui_tx
    .send(pointer_down(tab_id, (10.0, 10.0), PointerButton::Primary))
    .unwrap();
  ui_tx
    .send(pointer_up(tab_id, (10.0, 10.0), PointerButton::Primary))
    .unwrap();

  let deadline = Instant::now() + SELECT_DROPDOWN_TIMEOUT;
  let mut received = None;
  while Instant::now() < deadline {
    match ui_rx.recv_timeout(Duration::from_millis(200)) {
      Ok(msg) => match msg {
        WorkerToUi::SelectDropdownOpened {
          tab_id: msg_tab,
          select_node_id,
          control,
          ..
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

  let (select_node_id, control) = received.expect("expected SelectDropdownOpened message");
  assert!(select_node_id > 0, "expected non-zero select_node_id");
  assert!(
    !control.multiple,
    "expected dropdown select to be single-select"
  );
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
