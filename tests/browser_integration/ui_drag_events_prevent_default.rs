#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{PointerButton, PointerModifiers, RenderedFrame, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn next_frame_ready(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
) -> RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| matches!(msg, WorkerToUi::FrameReady { .. }))
    .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));
  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("unexpected message while waiting for FrameReady: {other:?}"),
  }
}

fn next_clipboard_text(rx: &impl support::RecvTimeout<WorkerToUi>, tab_id: TabId) -> String {
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
fn ui_drag_drop_drop_prevent_default_suppresses_default_text_insertion() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html><meta charset="utf-8"><style>html,body{margin:0;padding:0}input{position:absolute;top:0;width:180px;height:40px;padding:0;border:0;outline:none;font:24px "Noto Sans Mono",monospace}#src{left:0}#dst{left:220px;width:320px}</style><input id=src value=hello><input id=dst value=""><script>const src=document.getElementById("src");const dst=document.getElementById("dst");src.addEventListener("dragstart",ev=>{ev.dataTransfer.setData("text/plain","custom");});dst.addEventListener("dragover",ev=>{ev.preventDefault();});dst.addEventListener("drop",ev=>{dst.value="js:"+ev.dataTransfer.getData("text/plain");ev.preventDefault();});</script>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-drag-events-prevent-default",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
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

  // Initial paint ensures layout is ready before interaction-driven selection/drag-drop.
  let _ = next_frame_ready(&ui_rx, tab_id);

  // Focus the source input.
  let src_pos = (10.0, 20.0);
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: src_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("focus src down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: src_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("focus src up");

  // Select all text in the source input (so the subsequent drag is treated as a drag/drop of the
  // selection).
  ui_tx
    .send(UiToWorker::SelectAll { tab_id })
    .expect("select all in src input");

  // Drag the selection into the destination input.
  let dst_pos = (230.0, 20.0);
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: src_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("drag down");
  ui_tx
    .send(UiToWorker::PointerMove {
      tab_id,
      pos_css: dst_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("drag move");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: dst_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("drag up");

  // Ensure the destination input is focused so SelectAll/Copy targets its value.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: dst_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("focus dst down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: dst_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("focus dst up");

  ui_tx
    .send(UiToWorker::SelectAll { tab_id })
    .expect("select all in dst input");
  ui_tx.send(UiToWorker::Copy { tab_id }).expect("copy");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "js:custom");

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
