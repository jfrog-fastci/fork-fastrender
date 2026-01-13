#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  NavigationReason, PointerModifiers, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(20);

fn next_frame_ready(
  rx: &fastrender::ui::WorkerToUiInbox,
  tab_id: TabId,
) -> fastrender::ui::RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));
  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("unexpected message while waiting for FrameReady: {other:?}"),
  }
}

fn next_clipboard_text(rx: &fastrender::ui::WorkerToUiInbox, tab_id: TabId) -> String {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::SetClipboardText { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for SetClipboardText for tab {tab_id:?}"));
  match msg {
    WorkerToUi::SetClipboardText { text, .. } => text,
    other => panic!("unexpected message while waiting for SetClipboardText: {other:?}"),
  }
}

#[test]
fn ui_context_menu_text_caret_right_click_places_paste_at_click_position() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; background: rgb(0,0,0); }
      #t {
        position: absolute;
        left: 0;
        top: 0;
        width: 200px;
        height: 30px;
        border: 0;
        padding: 0;
        outline: none;
        font: 16px "Noto Sans Mono", monospace;
        background: rgb(255,255,255);
        color: rgb(0,0,0);
      }
      #probe {
        position: absolute;
        left: 0;
        top: 40px;
        width: 20px;
        height: 20px;
        background: rgb(255,0,0);
      }
      #t[value="Xhello"] + #probe { background: rgb(0,255,0); }
      #t[value="helloX"] + #probe { background: rgb(0,0,255); }
    </style>
  </head>
  <body>
    <input id="t" value="hello">
    <div id="probe"></div>
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-context-menu-text-caret-right-click")
    .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx.send(support::create_tab_msg(tab_id, None)).unwrap();
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (240, 80), 1.0))
    .unwrap();
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url,
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  // Initial paint so hit-testing works.
  let _ = next_frame_ready(&ui_rx, tab_id);

  // Right-click inside the input near the left edge.
  let pos_css = (1.0, 15.0);
  ui_tx
    .send(UiToWorker::ContextMenuRequest {
      tab_id,
      pos_css,
      modifiers: PointerModifiers::NONE,
    })
    .unwrap();
  let _ = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ContextMenu { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for ContextMenu for tab {tab_id:?}"));

  // Paste should insert at the click position (start), not the end.
  ui_tx
    .send(UiToWorker::Paste {
      tab_id,
      text: "X".to_string(),
    })
    .unwrap();

  let deadline = Instant::now() + TIMEOUT;
  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
      panic!("timed out waiting for probe to turn green after paste");
    }
    let msg = support::recv_for_tab(&ui_rx, tab_id, remaining, |msg| {
      matches!(msg, WorkerToUi::FrameReady { .. })
    })
    .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

    let frame = match msg {
      WorkerToUi::FrameReady { frame, .. } => frame,
      other => panic!("unexpected message while waiting for FrameReady: {other:?}"),
    };

    // Sample within the probe box.
    let rgba = support::rgba_at(&frame.pixmap, 5, 45);
    if rgba == [0, 255, 0, 255] {
      break;
    }
    if rgba == [0, 0, 255, 255] {
      panic!("paste inserted at the end of the input instead of the click position");
    }
    // Otherwise: likely a repaint from focusing/caret movement; keep waiting for the paste frame.
  }

  drop(ui_tx);
  join.join().unwrap();
}

#[test]
fn ui_context_menu_textarea_caret_right_click_places_paste_at_click_position() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255,255,255); }
      #t {
        position: absolute;
        left: 0;
        top: 0;
        width: 200px;
        height: 60px;
        border: 0;
        padding: 0;
        outline: none;
        font: 16px "Noto Sans Mono", monospace;
        background: rgb(255,255,255);
        color: rgb(0,0,0);
      }
    </style>
  </head>
  <body>
    <textarea id="t">hello</textarea>
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-context-menu-textarea-caret-right-click")
    .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx.send(support::create_tab_msg(tab_id, None)).unwrap();
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (240, 80), 1.0))
    .unwrap();
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url,
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  // Initial paint so hit-testing works.
  let _ = next_frame_ready(&ui_rx, tab_id);

  // Right-click inside the textarea near the left edge.
  let pos_css = (1.0, 15.0);
  ui_tx
    .send(UiToWorker::ContextMenuRequest {
      tab_id,
      pos_css,
      modifiers: PointerModifiers::NONE,
    })
    .unwrap();
  let _ = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ContextMenu { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for ContextMenu for tab {tab_id:?}"));

  // Paste should insert at the click position (start), not the end.
  ui_tx
    .send(UiToWorker::Paste {
      tab_id,
      text: "X".to_string(),
    })
    .unwrap();

  // Wait for the paste repaint.
  let _ = next_frame_ready(&ui_rx, tab_id);

  // Verify the value via SelectAll+Copy.
  ui_tx.send(UiToWorker::SelectAll { tab_id }).unwrap();
  ui_tx.send(UiToWorker::Copy { tab_id }).unwrap();
  let copied = next_clipboard_text(&ui_rx, tab_id);
  if copied == "helloX" {
    panic!("paste inserted at the end of the textarea instead of the click position");
  }
  assert_eq!(copied, "Xhello");

  drop(ui_tx);
  join.join().unwrap();
}
