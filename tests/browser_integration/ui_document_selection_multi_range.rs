#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  PointerButton, PointerModifiers, RenderedFrame, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::time::{Duration, Instant};

// UI worker tests exercise real worker threads and rendering; allow some slack on CI.
const TIMEOUT: Duration = Duration::from_secs(20);

// `rgba(0, 120, 215, 0.35)` over white.
//
// Different raster backends may round channel values slightly differently (e.g. 207 vs 208 for
// green). Accept a small tolerance so the test asserts selection behavior rather than the exact
// rounding mode.
const SELECTION_HIGHLIGHT: [u8; 4] = [166, 208, 241, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];

fn is_selection_highlight(rgba: [u8; 4]) -> bool {
  rgba[0] == SELECTION_HIGHLIGHT[0]
    && rgba[2] == SELECTION_HIGHLIGHT[2]
    && rgba[3] == SELECTION_HIGHLIGHT[3]
    && rgba[1].abs_diff(SELECTION_HIGHLIGHT[1]) <= 1
}

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

fn next_frame_ready_until(
  rx: &fastrender::ui::WorkerToUiInbox,
  tab_id: TabId,
  context: &'static str,
  mut pred: impl FnMut(&RenderedFrame) -> bool,
) -> RenderedFrame {
  let start = Instant::now();
  loop {
    let remaining = TIMEOUT.saturating_sub(start.elapsed());
    if remaining.is_zero() {
      panic!("timed out waiting for FrameReady ({context}) for tab {tab_id:?}");
    }

    let msg = support::recv_for_tab(rx, tab_id, remaining, |msg| {
      matches!(msg, WorkerToUi::FrameReady { .. })
    })
    .unwrap_or_else(|| panic!("timed out waiting for FrameReady ({context}) for tab {tab_id:?}"));
    let frame = match msg {
      WorkerToUi::FrameReady { frame, .. } => frame,
      other => panic!("unexpected message while waiting for FrameReady ({context}): {other:?}"),
    };
    if pred(&frame) {
      return frame;
    }
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
fn ui_document_selection_multi_range_ctrl_click_adds_ranges_and_copy_concatenates() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html><meta charset="utf-8">
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  body { font: 40px/80px monospace; }
  .word { display: inline-block; width: 200px; }
</style>
<span id="a" class="word">AAAAA</span><span id="b" class="word">BBBBB</span><span id="c" class="word">CCCCC</span>
"#,
  );

  let handle =
    spawn_ui_worker("fastr-ui-worker-document-selection-multi-range").expect("spawn ui worker");
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
      viewport_css: (700, 120),
      dpr: 1.0,
    })
    .expect("viewport");
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id })
    .expect("active tab");

  // Initial paint.
  let _ = next_frame_ready(&ui_rx, tab_id);

  // Drag-select the first word.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 40.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("pointer down (A)");
  ui_tx
    .send(UiToWorker::PointerMove {
      tab_id,
      pos_css: (110.0, 40.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer move (A)");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (110.0, 40.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer up (A)");

  let frame_a = next_frame_ready_until(&ui_rx, tab_id, "after selecting A", |frame| {
    is_selection_highlight(support::rgba_at(&frame.pixmap, 20, 10))
  });
  assert!(is_selection_highlight(support::rgba_at(
    &frame_a.pixmap,
    20,
    10
  )));
  assert_eq!(support::rgba_at(&frame_a.pixmap, 220, 10), WHITE);
  assert_eq!(support::rgba_at(&frame_a.pixmap, 420, 10), WHITE);

  // Ctrl/Cmd drag-select the second word, keeping the first range.
  let cmd_mods = PointerModifiers::CTRL | PointerModifiers::META;
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (210.0, 40.0),
      button: PointerButton::Primary,
      modifiers: cmd_mods,
      click_count: 1,
    })
    .expect("pointer down (B)");
  ui_tx
    .send(UiToWorker::PointerMove {
      tab_id,
      pos_css: (310.0, 40.0),
      button: PointerButton::Primary,
      modifiers: cmd_mods,
    })
    .expect("pointer move (B)");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (310.0, 40.0),
      button: PointerButton::Primary,
      modifiers: cmd_mods,
    })
    .expect("pointer up (B)");

  let frame_ab = next_frame_ready_until(&ui_rx, tab_id, "after selecting A+B", |frame| {
    is_selection_highlight(support::rgba_at(&frame.pixmap, 20, 10))
      && is_selection_highlight(support::rgba_at(&frame.pixmap, 220, 10))
      && support::rgba_at(&frame.pixmap, 420, 10) == WHITE
  });
  assert!(is_selection_highlight(support::rgba_at(
    &frame_ab.pixmap,
    20,
    10
  )));
  assert!(is_selection_highlight(support::rgba_at(
    &frame_ab.pixmap,
    220,
    10
  )));
  assert_eq!(support::rgba_at(&frame_ab.pixmap, 420, 10), WHITE);

  ui_tx.send(UiToWorker::Copy { tab_id }).expect("copy");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "AAAAA\nBBBBB");

  // Shift+click extends the primary (second) range without clearing the first.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (510.0, 40.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::SHIFT,
      click_count: 1,
    })
    .expect("pointer down (shift)");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (510.0, 40.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::SHIFT,
    })
    .expect("pointer up (shift)");

  let frame_abc = next_frame_ready_until(
    &ui_rx,
    tab_id,
    "after shift-extending primary range",
    |frame| {
      is_selection_highlight(support::rgba_at(&frame.pixmap, 20, 10))
        && is_selection_highlight(support::rgba_at(&frame.pixmap, 220, 10))
        && is_selection_highlight(support::rgba_at(&frame.pixmap, 420, 10))
    },
  );
  assert!(is_selection_highlight(support::rgba_at(
    &frame_abc.pixmap,
    20,
    10
  )));
  assert!(is_selection_highlight(support::rgba_at(
    &frame_abc.pixmap,
    220,
    10
  )));
  assert!(is_selection_highlight(support::rgba_at(
    &frame_abc.pixmap,
    420,
    10
  )));

  ui_tx.send(UiToWorker::Copy { tab_id }).expect("copy");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "AAAAA\nBBBBBCCCCC");

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
