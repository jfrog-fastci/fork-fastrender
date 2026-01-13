#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(20);

fn next_frame_ready(rx: &fastrender::ui::WorkerToUiInbox, tab_id: TabId) {
  let _msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));
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
fn ui_context_menu_preserves_text_selection() {
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
      html, body { margin: 0; padding: 0; }
      #i {
        position: absolute;
        left: 0;
        top: 0;
        width: 320px;
        height: 40px;
        padding: 0;
        border: 0;
        outline: none;
        font-family: "Noto Sans Mono", monospace;
        font-size: 24px;
      }
    </style>
  </head>
  <body>
    <input id="i" value="hello world">
  </body>
</html>
"#,
  );

  let handle =
    spawn_ui_worker("fastr-ui-worker-context-menu-preserve-selection").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: Some(url),
      cancel: Default::default(),
    })
    .expect("create tab");
  ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (360, 100),
      dpr: 1.0,
    })
    .expect("viewport");
  ui_tx.send(UiToWorker::SetActiveTab { tab_id }).unwrap();

  // Initial paint so context-menu hit-testing has cached layout artifacts.
  next_frame_ready(&ui_rx, tab_id);

  let input_pos = (10.0, 20.0);

  // Double-click selects the word "hello".
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: input_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("input click 1 down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: input_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("input click 1 up");
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: input_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 2,
    })
    .expect("input click 2 down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: input_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("input click 2 up");

  // Right-clicking inside the selected word should preserve the selection so Copy remains available.
  ui_tx
    .send(UiToWorker::ContextMenuRequest {
      tab_id,
      pos_css: input_pos,
      modifiers: PointerModifiers::NONE,
    })
    .expect("context menu request");
  let msg = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ContextMenu { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for ContextMenu for tab {tab_id:?}"));
  match msg {
    WorkerToUi::ContextMenu {
      tab_id: got_tab,
      pos_css: got_pos,
      link_url,
      can_copy,
      ..
    } => {
      assert_eq!(got_tab, tab_id);
      assert_eq!(got_pos, input_pos);
      assert_eq!(link_url, None);
      assert!(
        can_copy,
        "right-click inside selection should preserve can_copy"
      );
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  ui_tx.send(UiToWorker::Copy { tab_id }).expect("copy");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "hello");

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
