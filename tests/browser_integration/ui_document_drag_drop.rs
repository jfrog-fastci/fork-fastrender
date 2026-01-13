#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  PointerButton, PointerModifiers, RenderedFrame, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(20);

fn next_frame_ready(rx: &fastrender::ui::WorkerToUiInbox, tab_id: TabId) -> RenderedFrame {
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
fn ui_document_drag_drop_inserts_selection_into_text_control() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  // Minimize whitespace so SelectAll produces a stable selection payload.
  let url = site.write(
    "index.html",
    r#"<!doctype html><meta charset="utf-8"><style>html,body{margin:0;padding:0}#src{position:absolute;left:0;top:0;font:24px "Noto Sans Mono",monospace}#dst{position:absolute;left:200px;top:0;width:300px;height:40px;padding:0;border:0;outline:none;font:24px "Noto Sans Mono",monospace}</style><div id=src>hello</div><input id=dst>"#,
  );

  let handle =
    spawn_ui_worker("fastr-ui-worker-document-selection-drag-drop").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: Some(url.clone()),
      cancel: Default::default(),
    })
    .expect("create tab");
  ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (600, 120),
      dpr: 1.0,
    })
    .expect("viewport");
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id })
    .expect("active tab");

  // Initial paint (ensures cached layout for selection serialization).
  let _ = next_frame_ready(&ui_rx, tab_id);

  // No text control focused yet; SelectAll should create a document selection.
  ui_tx
    .send(UiToWorker::SelectAll { tab_id })
    .expect("select all");

  // Drag the document selection into the input.
  let src_pos = (10.0, 20.0);
  let dst_pos = (210.0, 20.0);
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: src_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("pointer down");
  ui_tx
    .send(UiToWorker::PointerMove {
      tab_id,
      pos_css: dst_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer move");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: dst_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer up");

  // Ensure the destination input is focused (and clear any document selection fallbacks).
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: dst_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("focus input down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: dst_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("focus input up");

  ui_tx
    .send(UiToWorker::SelectAll { tab_id })
    .expect("select all in input");
  ui_tx.send(UiToWorker::Copy { tab_id }).expect("copy");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "hello");

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
