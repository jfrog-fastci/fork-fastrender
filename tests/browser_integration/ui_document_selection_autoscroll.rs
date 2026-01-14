#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::scroll::ScrollState;
use fastrender::ui::messages::{
  PointerButton, PointerModifiers, RenderedFrame, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

// UI worker tests exercise real worker threads and rendering; allow some slack on CI.
const TIMEOUT: Duration = Duration::from_secs(20);

fn next_frame_ready(rx: &impl support::RecvTimeout<WorkerToUi>, tab_id: TabId) -> RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));
  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("unexpected message while waiting for FrameReady: {other:?}"),
  }
}

fn next_scroll_state(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
  pred: impl Fn(&ScrollState) -> bool,
) -> ScrollState {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| match msg {
    WorkerToUi::ScrollStateUpdated { scroll, .. } => pred(scroll),
    _ => false,
  })
  .unwrap_or_else(|| panic!("timed out waiting for ScrollStateUpdated for tab {tab_id:?}"));
  match msg {
    WorkerToUi::ScrollStateUpdated { scroll, .. } => scroll,
    other => panic!("unexpected message while waiting for ScrollStateUpdated: {other:?}"),
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
fn ui_document_selection_drag_autoscrolls_viewport_and_advances_selection() {
  let _lock = super::stage_listener_test_lock();

  let mut lines = String::new();
  for i in 1..=120 {
    lines.push_str(&format!("<p>Line {i:02}</p>\n"));
  }

  let html = format!(
    r#"<!doctype html><meta charset="utf-8">
<style>
  html, body {{ margin: 0; padding: 0; background: #fff; }}
  body {{ font: 16px/20px monospace; }}
  p {{ margin: 0; }}
</style>
{lines}
"#
  );

  let site = support::TempSite::new();
  let url = site.write("index.html", &html);

  let handle =
    spawn_ui_worker("fastr-ui-worker-document-selection-autoscroll").expect("spawn ui worker");
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
      viewport_css: (240, 100),
      dpr: 1.0,
    })
    .expect("viewport");
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id })
    .expect("active tab");

  // Ensure layout cache exists (required for hit-testing + selection serialization).
  let _ = next_frame_ready(&ui_rx, tab_id);

  // Start a document selection drag on the first line of text.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("pointer down");

  // Move near the bottom edge to trigger viewport autoscroll.
  ui_tx
    .send(UiToWorker::PointerMove {
      tab_id,
      pos_css: (150.0, 99.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer move");

  let first_scroll = next_scroll_state(&ui_rx, tab_id, |scroll| scroll.viewport.y > 0.0);

  // Copy while still dragging: selection should already have advanced into the newly scrolled
  // content (i.e. not lag until the next pointer move).
  ui_tx.send(UiToWorker::Copy { tab_id }).expect("copy");
  let copied = next_clipboard_text(&ui_rx, tab_id);
  assert!(
    copied.contains("Line 06"),
    "expected selection to extend into scrolled content after first autoscroll (scroll_y={}), got clipboard={copied:?}",
    first_scroll.viewport.y
  );

  // Continue dragging near the bottom edge: repeated pointer moves should keep autoscrolling.
  for _ in 0..5 {
    ui_tx
      .send(UiToWorker::PointerMove {
        tab_id,
        pos_css: (150.0, 99.0),
        button: PointerButton::Primary,
        modifiers: PointerModifiers::NONE,
      })
      .expect("pointer move (repeat)");
  }

  let later_scroll = next_scroll_state(&ui_rx, tab_id, |scroll| {
    scroll.viewport.y > first_scroll.viewport.y
  });
  assert!(
    later_scroll.viewport.y > first_scroll.viewport.y,
    "expected repeated autoscroll moves to increase scroll_y (before={}, after={})",
    first_scroll.viewport.y,
    later_scroll.viewport.y
  );

  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (150.0, 99.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer up");

  drop(ui_tx);
  join.join().expect("join ui worker");
}
