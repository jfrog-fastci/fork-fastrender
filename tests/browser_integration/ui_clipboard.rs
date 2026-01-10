#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  KeyAction, PointerButton, PointerModifiers, RenderedFrame, RepaintReason, TabId, UiToWorker,
  WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::Duration;

// Clipboard tests exercise real worker threads and rendering; allow some slack on CI.
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
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| matches!(msg, WorkerToUi::SetClipboardText { .. }))
    .unwrap_or_else(|| panic!("timed out waiting for SetClipboardText for tab {tab_id:?}"));
  match msg {
    WorkerToUi::SetClipboardText { text, .. } => text,
    other => panic!("unexpected message while waiting for SetClipboardText: {other:?}"),
  }
}

#[test]
fn ui_clipboard_copy_cut_paste_for_focused_input() {
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

      #t {
        position: absolute;
        left: 10px;
        top: 10px;
        width: 180px;
        height: 30px;
      }

      /* A probe element whose background is driven by the input's current value attribute. */
      #probe {
        position: absolute;
        left: 10px;
        top: 50px;
        width: 20px;
        height: 20px;
        background: rgb(0, 0, 0);
      }

      #t[value="hello"] + #probe { background: rgb(0, 255, 0); }
      #t[value=""] + #probe { background: rgb(255, 0, 0); }
      #t[value="world"] + #probe { background: rgb(0, 0, 255); }
    </style>
  </head>
  <body>
    <input id="t" type="text" value="hello"><div id="probe"></div>
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-clipboard-input").expect("spawn ui worker");
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
      viewport_css: (200, 100),
      dpr: 1.0,
    })
    .expect("viewport");
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id })
    .expect("active tab");

  // Wait for the initial paint and assert the probe is green (value="hello").
  let frame0 = next_frame_ready(&ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame0.pixmap, 20, 60),
    [0, 255, 0, 255],
    "expected probe to reflect initial input value"
  );

  // Click the input to focus it.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (15.0, 15.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (15.0, 15.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer up");
  let _ = next_frame_ready(&ui_rx, tab_id);

  // Select all, then copy.
  ui_tx
    .send(UiToWorker::SelectAll { tab_id })
    .expect("select all");
  ui_tx.send(UiToWorker::Copy { tab_id }).expect("copy");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "hello");

  // Cut: should set clipboard and clear the input value (probe turns red).
  ui_tx.send(UiToWorker::Cut { tab_id }).expect("cut");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "hello");
  let frame_cut = next_frame_ready(&ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame_cut.pixmap, 20, 60),
    [255, 0, 0, 255],
    "expected probe to reflect value after cut"
  );

  // Paste: should insert at the caret and update the value (probe turns blue).
  ui_tx
    .send(UiToWorker::Paste {
      tab_id,
      text: "world".to_string(),
    })
    .expect("paste");
  let frame_paste = next_frame_ready(&ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame_paste.pixmap, 20, 60),
    [0, 0, 255, 255],
    "expected probe to reflect value after paste"
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

#[test]
fn ui_clipboard_copy_cut_paste_for_focused_textarea() {
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

      #t {
        position: absolute;
        left: 10px;
        top: 10px;
        width: 180px;
        height: 50px;
      }

      /* A probe element whose background reflects whether the textarea is empty. */
      #probe {
        position: absolute;
        left: 10px;
        top: 70px;
        width: 20px;
        height: 20px;
        background: rgb(0, 0, 0);
      }

      #t:placeholder-shown + #probe { background: rgb(255, 0, 0); }
      #t:not(:placeholder-shown) + #probe { background: rgb(0, 255, 0); }
    </style>
  </head>
  <body>
    <textarea id="t" placeholder="x">hello</textarea><div id="probe"></div>
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-clipboard-textarea").expect("spawn ui worker");
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
      viewport_css: (200, 120),
      dpr: 1.0,
    })
    .expect("viewport");
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id })
    .expect("active tab");

  // Initial value is non-empty, so the probe should be green.
  let frame0 = next_frame_ready(&ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame0.pixmap, 20, 80),
    [0, 255, 0, 255],
    "expected probe to reflect initial textarea value"
  );

  // Click the textarea to focus it.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (15.0, 15.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (15.0, 15.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer up");
  let _ = next_frame_ready(&ui_rx, tab_id);

  // Select all, then copy.
  ui_tx
    .send(UiToWorker::SelectAll { tab_id })
    .expect("select all");
  ui_tx.send(UiToWorker::Copy { tab_id }).expect("copy");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "hello");

  // Cut: should set clipboard and clear the textarea (probe turns red).
  ui_tx.send(UiToWorker::Cut { tab_id }).expect("cut");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "hello");
  let frame_cut = next_frame_ready(&ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame_cut.pixmap, 20, 80),
    [255, 0, 0, 255],
    "expected probe to reflect value after cut"
  );

  // Paste: should insert at the caret and make textarea non-empty again.
  ui_tx
    .send(UiToWorker::Paste {
      tab_id,
      text: "world".to_string(),
    })
    .expect("paste");
  let frame_paste = next_frame_ready(&ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame_paste.pixmap, 20, 80),
    [0, 255, 0, 255],
    "expected probe to reflect value after paste"
  );

  // Copy again to ensure the pasted text landed in the DOM.
  ui_tx
    .send(UiToWorker::SelectAll { tab_id })
    .expect("select all after paste");
  ui_tx
    .send(UiToWorker::Copy { tab_id })
    .expect("copy after paste");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "world");

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

#[test]
fn ui_clipboard_respects_readonly_input() {
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

      #t {
        position: absolute;
        left: 10px;
        top: 10px;
        width: 180px;
        height: 30px;
      }

      /* A probe element whose background is driven by the input's current value attribute. */
      #probe {
        position: absolute;
        left: 10px;
        top: 50px;
        width: 20px;
        height: 20px;
        background: rgb(0, 0, 0);
      }

      #t[value="hello"] + #probe { background: rgb(0, 255, 0); }
      #t[value=""] + #probe { background: rgb(255, 0, 0); }
      #t[value="world"] + #probe { background: rgb(0, 0, 255); }
    </style>
  </head>
  <body>
    <input id="t" type="text" value="hello" readonly><div id="probe"></div>
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-clipboard-readonly-input").expect("spawn ui worker");
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
      viewport_css: (200, 100),
      dpr: 1.0,
    })
    .expect("viewport");
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id })
    .expect("active tab");

  // Wait for the initial paint and assert the probe is green (value="hello").
  let frame0 = next_frame_ready(&ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame0.pixmap, 20, 60),
    [0, 255, 0, 255],
    "expected probe to reflect initial readonly input value"
  );

  // Click the input to focus it.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (15.0, 15.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (15.0, 15.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer up");
  let _ = next_frame_ready(&ui_rx, tab_id);

  // Select all.
  ui_tx
    .send(UiToWorker::SelectAll { tab_id })
    .expect("select all");

  // Cut: should still write to clipboard, but must not mutate the readonly value.
  ui_tx.send(UiToWorker::Cut { tab_id }).expect("cut");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "hello");
  ui_tx
    .send(UiToWorker::RequestRepaint {
      tab_id,
      reason: RepaintReason::Explicit,
    })
    .expect("request repaint after cut");
  let frame_cut = next_frame_ready(&ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame_cut.pixmap, 20, 60),
    [0, 255, 0, 255],
    "expected readonly input value to remain unchanged after cut"
  );

  // Paste: should be ignored for readonly controls.
  ui_tx
    .send(UiToWorker::Paste {
      tab_id,
      text: "world".to_string(),
    })
    .expect("paste");
  ui_tx
    .send(UiToWorker::RequestRepaint {
      tab_id,
      reason: RepaintReason::Explicit,
    })
    .expect("request repaint after paste");
  let frame_paste = next_frame_ready(&ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame_paste.pixmap, 20, 60),
    [0, 255, 0, 255],
    "expected readonly input value to remain unchanged after paste"
  );

  // Copy again to ensure the value stayed intact.
  ui_tx
    .send(UiToWorker::SelectAll { tab_id })
    .expect("select all after paste");
  ui_tx.send(UiToWorker::Copy { tab_id }).expect("copy");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "hello");

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

#[test]
fn ui_clipboard_copy_cut_respects_selection() {
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

      #t {
        position: absolute;
        left: 10px;
        top: 10px;
        width: 180px;
        height: 30px;
      }

      /* A probe element whose background is driven by the input's current value attribute. */
      #probe {
        position: absolute;
        left: 10px;
        top: 50px;
        width: 20px;
        height: 20px;
        background: rgb(0, 0, 0);
      }

      #t[value="hello"] + #probe { background: rgb(0, 255, 0); }
      #t[value="o"] + #probe { background: rgb(255, 255, 0); }
      #t[value=""] + #probe { background: rgb(255, 0, 0); }
    </style>
  </head>
  <body>
    <input id="t" type="text" value="hello"><div id="probe"></div>
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-clipboard-selection").expect("spawn ui worker");
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
      viewport_css: (200, 100),
      dpr: 1.0,
    })
    .expect("viewport");
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id })
    .expect("active tab");

  // Wait for the initial paint and assert the probe is green (value="hello").
  let frame0 = next_frame_ready(&ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame0.pixmap, 20, 60),
    [0, 255, 0, 255],
    "expected probe to reflect initial input value"
  );

  // Click the input to focus it.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (15.0, 15.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (15.0, 15.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer up");
  let _ = next_frame_ready(&ui_rx, tab_id);

  // Select all, then shrink the selection by one character so the selected text is "hell".
  ui_tx
    .send(UiToWorker::SelectAll { tab_id })
    .expect("select all");
  ui_tx
    .send(UiToWorker::KeyAction {
      tab_id,
      key: KeyAction::ShiftArrowLeft,
    })
    .expect("shift arrow left");

  // Copy should copy the selection, not the full value.
  ui_tx.send(UiToWorker::Copy { tab_id }).expect("copy");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "hell");

  // Cut should copy the selection and delete it, leaving only the last character (value="o").
  ui_tx.send(UiToWorker::Cut { tab_id }).expect("cut");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "hell");
  let frame_cut = next_frame_ready(&ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame_cut.pixmap, 20, 60),
    [255, 255, 0, 255],
    "expected probe to reflect value after cutting selection"
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
