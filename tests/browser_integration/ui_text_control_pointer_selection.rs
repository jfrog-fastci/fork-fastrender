#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  KeyAction, PointerButton, PointerModifiers, RenderedFrame, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(20);

fn next_frame_ready(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| matches!(msg, WorkerToUi::FrameReady { .. }))
    .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));
  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("unexpected message while waiting for FrameReady: {other:?}"),
  }
}

fn next_clipboard_text(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> String {
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
fn ui_text_control_pointer_selection_double_triple_shift_click() {
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
      #ta {
        position: absolute;
        left: 0;
        top: 60px;
        width: 320px;
        height: 90px;
        padding: 0;
        border: 0;
        outline: none;
        font-family: "Noto Sans Mono", monospace;
        font-size: 24px;
      }
    </style>
  </head>
  <body>
    <input id="i" value="hello world again">
    <textarea id="ta">alpha beta
second line</textarea>
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-text-control-pointer-selection").expect("spawn ui worker");
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
      viewport_css: (360, 200),
      dpr: 1.0,
    })
    .expect("viewport");
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id })
    .expect("active tab");

  // Initial paint.
  let _ = next_frame_ready(&ui_rx, tab_id);

  let input_pos = (10.0, 20.0);
  let input_far_right = (310.0, 20.0);
  let input_mid_world = (120.0, 20.0);

  // Double-click selects word.
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

  ui_tx.send(UiToWorker::Copy { tab_id }).expect("copy word");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "hello");

  // Triple-click selects all.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: input_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 3,
    })
    .expect("input click 3 down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: input_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("input click 3 up");
  ui_tx.send(UiToWorker::Copy { tab_id }).expect("copy all");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "hello world again");

  // Shift-click extends selection.
  ui_tx
    .send(UiToWorker::KeyAction {
      tab_id,
      key: KeyAction::Home,
    })
    .expect("home");
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: input_far_right,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::SHIFT,
      click_count: 1,
    })
    .expect("input shift click down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: input_far_right,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::SHIFT,
    })
    .expect("input shift click up");
  ui_tx
    .send(UiToWorker::Copy { tab_id })
    .expect("copy after shift click");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "hello world again");

  // Textarea: double-click selects word; triple-click selects line.
  let textarea_pos = (10.0, 80.0);
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: textarea_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("textarea click 1 down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: textarea_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("textarea click 1 up");
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: textarea_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 2,
    })
    .expect("textarea click 2 down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: textarea_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("textarea click 2 up");
  ui_tx
    .send(UiToWorker::Copy { tab_id })
    .expect("copy textarea word");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "alpha");

  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: textarea_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 3,
    })
    .expect("textarea click 3 down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: textarea_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("textarea click 3 up");
  ui_tx
    .send(UiToWorker::Copy { tab_id })
    .expect("copy textarea line");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "alpha beta");

  // Double-click + drag extends selection by whole words.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: input_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("input drag focus down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: input_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("input drag focus up");
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: input_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 2,
    })
    .expect("input double click down for drag");
  ui_tx
    .send(UiToWorker::PointerMove {
      tab_id,
      pos_css: input_mid_world,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("input drag move");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: input_mid_world,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("input drag up");
  ui_tx.send(UiToWorker::Copy { tab_id }).expect("copy word drag");
  assert_eq!(
    next_clipboard_text(&ui_rx, tab_id),
    "hello world",
    "word drag should extend to full word boundaries"
  );

  // Textarea triple-click + drag extends selection by whole lines.
  let textarea_mid_second_line = (100.0, 120.0);
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: textarea_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("textarea drag focus down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: textarea_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("textarea drag focus up");
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: textarea_pos,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 3,
    })
    .expect("textarea triple click down for drag");
  ui_tx
    .send(UiToWorker::PointerMove {
      tab_id,
      pos_css: textarea_mid_second_line,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("textarea drag move");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: textarea_mid_second_line,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("textarea drag up");
  ui_tx
    .send(UiToWorker::Copy { tab_id })
    .expect("copy textarea line drag");
  assert_eq!(
    next_clipboard_text(&ui_rx, tab_id),
    "alpha beta\nsecond line",
    "line drag should extend to full line boundaries"
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

#[test]
fn ui_text_control_pointer_selection_double_click_drag_preserves_initial_word() {
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
    spawn_ui_worker("fastr-ui-worker-text-control-pointer-multiclick-drag").expect("spawn ui worker");
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
      viewport_css: (360, 120),
      dpr: 1.0,
    })
    .expect("viewport");
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id })
    .expect("active tab");

  // Initial paint.
  let _ = next_frame_ready(&ui_rx, tab_id);

  let input_left = (10.0, 20.0);
  let input_right = (310.0, 20.0);

  // Focus the input first (multi-click selection is suppressed for the first click that focuses).
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: input_left,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("focus input down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: input_left,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("focus input up");

  // Double-click selects the second word; drag left should preserve the initially selected word.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: input_right,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 2,
    })
    .expect("double click down");
  ui_tx
    .send(UiToWorker::PointerMove {
      tab_id,
      pos_css: input_left,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("drag left");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: input_left,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("double click drag up");

  ui_tx.send(UiToWorker::Copy { tab_id }).expect("copy");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "hello world");

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
