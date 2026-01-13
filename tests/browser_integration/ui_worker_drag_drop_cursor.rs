#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{CursorKind, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi};
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
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| matches!(msg, WorkerToUi::HoverChanged { .. }))
    .unwrap_or_else(|| panic!("timed out waiting for HoverChanged for tab {tab_id:?}"));
  match msg {
    WorkerToUi::HoverChanged { cursor, .. } => cursor,
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

#[test]
fn ui_worker_drag_drop_reports_grabbing_vs_not_allowed_cursor() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    // Minimize whitespace so SelectAll yields stable selection and hit-testing positions.
    r#"<!doctype html><meta charset="utf-8"><style>html,body{margin:0;padding:0}#src{position:absolute;left:0;top:0;margin:0;font:24px "Noto Sans Mono",monospace}#dst{position:absolute;left:200px;top:0;width:240px;height:40px;padding:0;border:0;outline:none;font:24px "Noto Sans Mono",monospace}</style><p id=src>hello</p><input id=dst>"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-drag-drop-cursor").expect("spawn ui worker");
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
      viewport_css: (600, 120),
      dpr: 1.0,
    })
    .expect("viewport");
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id })
    .expect("active tab");

  next_frame_ready(&ui_rx, tab_id);

  // Create a document selection.
  ui_tx.send(UiToWorker::SelectAll { tab_id }).expect("select all");

  let src_pos = (10.0, 20.0);
  let dst_pos = (210.0, 20.0);

  // Begin a drag-drop gesture from the document selection.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: src_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("pointer down");

  // Move far enough to activate drag-drop and hover over the input: cursor should be "grabbing".
  ui_tx
    .send(UiToWorker::PointerMove {
      tab_id,
      pos_css: dst_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer move");
  assert_eq!(next_hover_cursor(&ui_rx, tab_id), CursorKind::Grabbing);

  // Hover over a non-droppable element while dragging: cursor should become "not-allowed".
  ui_tx
    .send(UiToWorker::PointerMove {
      tab_id,
      pos_css: src_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer move");
  assert_eq!(next_hover_cursor(&ui_rx, tab_id), CursorKind::NotAllowed);

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
