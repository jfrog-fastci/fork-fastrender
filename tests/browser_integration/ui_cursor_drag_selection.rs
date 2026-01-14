#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  CursorKind, NavigationReason, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn next_frame_ready(rx: &impl support::RecvTimeout<WorkerToUi>, tab_id: TabId) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  if let WorkerToUi::NavigationFailed { url, error, .. } = msg {
    panic!("navigation failed for {url}: {error}");
  }
}

fn next_hover_cursor(rx: &impl support::RecvTimeout<WorkerToUi>, tab_id: TabId) -> CursorKind {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::HoverChanged { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for HoverChanged for tab {tab_id:?}"));
  match msg {
    WorkerToUi::HoverChanged { cursor, .. } => cursor,
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

#[test]
fn ui_cursor_drag_document_selection_uses_grab_and_grabbing() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page_url = site.write(
    "index.html",
    // Minimize whitespace so hit-testing positions are stable.
    r#"<!doctype html><meta charset="utf-8"><style>html,body{margin:0;padding:0}#src{position:absolute;left:0;top:0;margin:0;font:24px "Noto Sans Mono",monospace}#dst{position:absolute;left:200px;top:0;width:240px;height:40px;padding:0;border:0;outline:none;font:24px "Noto Sans Mono",monospace}</style><p id=src>hello</p><input id=dst value=drop>"#,
  );

  let worker =
    spawn_ui_worker("fastr-ui-worker-cursor-drag-document-selection").expect("spawn ui worker");
  let tab_id = TabId::new();
  worker.ui_tx.send(support::create_tab_msg(tab_id, None)).unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (520, 120), 1.0))
    .unwrap();
  worker
    .ui_tx
    .send(support::navigate_msg(tab_id, page_url, NavigationReason::TypedUrl))
    .unwrap();
  next_frame_ready(&worker.ui_rx, tab_id);

  let src_pos = (10.0, 20.0);
  let dst_pos = (210.0, 20.0);

  // No selection: selectable text uses the I-beam.
  worker
    .ui_tx
    .send(support::pointer_move(tab_id, src_pos, PointerButton::None))
    .unwrap();
  assert_eq!(next_hover_cursor(&worker.ui_rx, tab_id), CursorKind::Text);

  // Create a document selection.
  worker.ui_tx.send(UiToWorker::SelectAll { tab_id }).unwrap();

  // Hover inside the highlighted selection: should show Grab.
  worker
    .ui_tx
    .send(support::pointer_move(tab_id, src_pos, PointerButton::None))
    .unwrap();
  assert_eq!(next_hover_cursor(&worker.ui_rx, tab_id), CursorKind::Grab);

  // Ensure we don't show Grab just because a document selection exists: hovering a text input should
  // still show the I-beam.
  worker
    .ui_tx
    .send(support::pointer_move(tab_id, dst_pos, PointerButton::None))
    .unwrap();
  assert_eq!(next_hover_cursor(&worker.ui_rx, tab_id), CursorKind::Text);

  // Begin dragging the selected text and hover over the input: cursor should become Grabbing.
  worker
    .ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: src_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .unwrap();
  worker
    .ui_tx
    .send(support::pointer_move(tab_id, dst_pos, PointerButton::Primary))
    .unwrap();
  assert_eq!(next_hover_cursor(&worker.ui_rx, tab_id), CursorKind::Grabbing);

  worker
    .ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: dst_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .unwrap();

  worker.join().unwrap();
}

#[test]
fn ui_cursor_drag_text_control_selection_uses_grab_and_grabbing() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page_url = site.write(
    "index.html",
    // Minimize whitespace so hit-testing positions are stable.
    r#"<!doctype html><meta charset="utf-8"><style>html,body{margin:0;padding:0}#src{position:absolute;left:0;top:0;width:180px;height:40px;padding:0;border:0;outline:none;font:24px "Noto Sans Mono",monospace}#dst{position:absolute;left:220px;top:0;width:180px;height:40px;padding:0;border:0;outline:none;font:24px "Noto Sans Mono",monospace}</style><input id=src value=hello><input id=dst value=world>"#,
  );

  let worker =
    spawn_ui_worker("fastr-ui-worker-cursor-drag-text-selection").expect("spawn ui worker");
  let tab_id = TabId::new();
  worker.ui_tx.send(support::create_tab_msg(tab_id, None)).unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (320, 120), 1.0))
    .unwrap();
  worker
    .ui_tx
    .send(support::navigate_msg(tab_id, page_url, NavigationReason::TypedUrl))
    .unwrap();
  next_frame_ready(&worker.ui_rx, tab_id);

  let src_pos = (10.0, 20.0);
  let dst_pos = (230.0, 20.0);

  // Focus the input.
  worker
    .ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: src_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .unwrap();
  worker
    .ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: src_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .unwrap();

  // No selection: hovering a text input uses the I-beam.
  worker
    .ui_tx
    .send(support::pointer_move(tab_id, src_pos, PointerButton::None))
    .unwrap();
  assert_eq!(next_hover_cursor(&worker.ui_rx, tab_id), CursorKind::Text);

  // Select all text in the focused input.
  worker.ui_tx.send(UiToWorker::SelectAll { tab_id }).unwrap();

  // Hover inside the highlighted selection: should show Grab (not the I-beam).
  worker
    .ui_tx
    .send(support::pointer_move(tab_id, src_pos, PointerButton::None))
    .unwrap();
  assert_eq!(next_hover_cursor(&worker.ui_rx, tab_id), CursorKind::Grab);

  // Drag the selection over another editable text control: cursor should show Grabbing.
  worker
    .ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: src_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .unwrap();
  worker
    .ui_tx
    .send(support::pointer_move(tab_id, dst_pos, PointerButton::Primary))
    .unwrap();
  assert_eq!(next_hover_cursor(&worker.ui_rx, tab_id), CursorKind::Grabbing);

  worker
    .ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: dst_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .unwrap();

  // After dropping, normal hover cursor should resume.
  worker
    .ui_tx
    .send(support::pointer_move(tab_id, dst_pos, PointerButton::None))
    .unwrap();
  assert_eq!(next_hover_cursor(&worker.ui_rx, tab_id), CursorKind::Text);

  worker.join().unwrap();
}
