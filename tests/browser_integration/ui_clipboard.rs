#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  KeyAction, PointerButton, PointerModifiers, RenderedFrame, RepaintReason, TabId, UiToWorker,
  WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::time::{Duration, Instant};

// Clipboard tests exercise real worker threads and rendering; allow some slack on CI.
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

fn next_frame_ready_with_probe_color(
  rx: &fastrender::ui::WorkerToUiInbox,
  tab_id: TabId,
  probe: (u32, u32),
  expected: [u8; 4],
  context: &'static str,
) -> RenderedFrame {
  next_frame_ready_with_probe_predicate(rx, tab_id, probe, context, |got| got == expected)
}

fn next_frame_ready_with_probe_predicate<F>(
  rx: &fastrender::ui::WorkerToUiInbox,
  tab_id: TabId,
  probe: (u32, u32),
  context: &'static str,
  mut pred: F,
) -> RenderedFrame
where
  F: FnMut([u8; 4]) -> bool,
{
  let start = Instant::now();
  let mut last_probe = None;
  loop {
    let remaining = TIMEOUT.saturating_sub(start.elapsed());
    if remaining.is_zero() {
      panic!(
        "timed out waiting for FrameReady ({context}) for tab {tab_id:?}; last probe at {:?} was {last_probe:?}",
        probe
      );
    }
    let msg = support::recv_for_tab(rx, tab_id, remaining, |msg| {
      matches!(msg, WorkerToUi::FrameReady { .. })
    })
    .unwrap_or_else(|| panic!("timed out waiting for FrameReady ({context}) for tab {tab_id:?}"));
    let frame = match msg {
      WorkerToUi::FrameReady { frame, .. } => frame,
      other => panic!("unexpected message while waiting for FrameReady ({context}): {other:?}"),
    };
    let got = support::rgba_at(&frame.pixmap, probe.0, probe.1);
    last_probe = Some(got);
    if pred(got) {
      return frame;
    }
  }
}

#[test]
fn ui_clipboard_copy_cut_paste_for_focused_input() {
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
  let _frame0 =
    next_frame_ready_with_probe_color(&ui_rx, tab_id, (20, 60), [0, 255, 0, 255], "initial paint");

  // Click the input to focus it.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (15.0, 15.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
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
  let _frame_cut =
    next_frame_ready_with_probe_color(&ui_rx, tab_id, (20, 60), [255, 0, 0, 255], "after cut");

  // Paste: should insert at the caret and update the value (probe turns blue).
  ui_tx
    .send(UiToWorker::Paste {
      tab_id,
      text: "world".to_string(),
    })
    .expect("paste");
  let _frame_paste =
    next_frame_ready_with_probe_color(&ui_rx, tab_id, (20, 60), [0, 0, 255, 255], "after paste");

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

#[test]
fn ui_clipboard_copy_cut_paste_for_focused_textarea() {
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
  let _frame0 =
    next_frame_ready_with_probe_color(&ui_rx, tab_id, (20, 80), [0, 255, 0, 255], "initial paint");

  // Click the textarea to focus it.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (15.0, 15.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
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
  let _frame_cut =
    next_frame_ready_with_probe_color(&ui_rx, tab_id, (20, 80), [255, 0, 0, 255], "after cut");

  // Paste: should insert at the caret and make textarea non-empty again.
  ui_tx
    .send(UiToWorker::Paste {
      tab_id,
      text: "world".to_string(),
    })
    .expect("paste");
  let _frame_paste =
    next_frame_ready_with_probe_color(&ui_rx, tab_id, (20, 80), [0, 255, 0, 255], "after paste");

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

  let handle =
    spawn_ui_worker("fastr-ui-worker-clipboard-readonly-input").expect("spawn ui worker");
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
  let _frame0 = next_frame_ready_with_probe_color(
    &ui_rx,
    tab_id,
    (20, 60),
    [0, 255, 0, 255],
    "initial paint (readonly)",
  );

  // Click the input to focus it.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (15.0, 15.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
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
  let _frame_cut = next_frame_ready_with_probe_color(
    &ui_rx,
    tab_id,
    (20, 60),
    [0, 255, 0, 255],
    "after cut (readonly)",
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
  let _frame_paste = next_frame_ready_with_probe_color(
    &ui_rx,
    tab_id,
    (20, 60),
    [0, 255, 0, 255],
    "after paste (readonly)",
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
  let _frame0 =
    next_frame_ready_with_probe_color(&ui_rx, tab_id, (20, 60), [0, 255, 0, 255], "initial paint");

  // Click the input to focus it.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (15.0, 15.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
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
  let _frame_cut = next_frame_ready_with_probe_color(
    &ui_rx,
    tab_id,
    (20, 60),
    [255, 255, 0, 255],
    "after cut selection",
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

#[test]
fn ui_clipboard_paste_replaces_selection() {
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
      #t[value="world"] + #probe { background: rgb(0, 0, 255); }
      /* If paste incorrectly inserts at the caret instead of replacing the selection, we'd end up with "helloworld". */
      #t[value="helloworld"] + #probe { background: rgb(255, 0, 0); }
    </style>
  </head>
  <body>
    <input id="t" type="text" value="hello"><div id="probe"></div>
  </body>
</html>
"#,
  );

  let handle =
    spawn_ui_worker("fastr-ui-worker-clipboard-paste-replace-selection").expect("spawn ui worker");
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
  let _frame0 =
    next_frame_ready_with_probe_color(&ui_rx, tab_id, (20, 60), [0, 255, 0, 255], "initial paint");

  // Click the input to focus it.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (15.0, 15.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
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

  // Select all and copy to confirm the selection is active.
  ui_tx
    .send(UiToWorker::SelectAll { tab_id })
    .expect("select all");
  ui_tx.send(UiToWorker::Copy { tab_id }).expect("copy");
  assert_eq!(next_clipboard_text(&ui_rx, tab_id), "hello");

  // Paste should replace the selection, yielding exactly value="world" (probe turns blue).
  ui_tx
    .send(UiToWorker::Paste {
      tab_id,
      text: "world".to_string(),
    })
    .expect("paste");
  let _frame_paste = next_frame_ready_with_probe_color(
    &ui_rx,
    tab_id,
    (20, 60),
    [0, 0, 255, 255],
    "after paste replace selection",
  );

  // Copy again to ensure the pasted value landed in the DOM.
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
fn ui_select_all_renders_selection_highlight() {
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
      html, body { margin: 0; padding: 0; background: rgb(255, 255, 255); }

      #t {
        position: absolute;
        left: 0;
        top: 0;
        width: 200px;
        height: 60px;
        padding: 0;
        border: none;
        outline: none;
        /* Avoid the browser-UI focus tint affecting our selection-highlight probe pixels. */
        accent-color: rgb(0, 0, 0);
        background: rgb(0, 0, 0);
        /* Use spaces so the selection highlight is visible without glyphs affecting pixels. */
        font: 24px/1 sans-serif;
        color: rgb(0, 0, 0);
      }
    </style>
  </head>
  <body>
    <input id="t" type="text" value="                    ">
  </body>
</html>
"#,
  );

  let handle =
    spawn_ui_worker("fastr-ui-worker-select-all-selection-highlight").expect("spawn ui worker");
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
      viewport_css: (220, 80),
      dpr: 1.0,
    })
    .expect("viewport");
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id })
    .expect("active tab");

  // Initial paint: sample inside the input background (black).
  let _frame0 =
    next_frame_ready_with_probe_color(&ui_rx, tab_id, (20, 30), [0, 0, 0, 255], "initial paint");

  // Focus the input using keyboard Tab traversal so this test doesn't depend on hit-testing.
  ui_tx
    .send(UiToWorker::KeyAction {
      tab_id,
      key: KeyAction::Tab,
    })
    .expect("tab focus");
  // Wait for the focus change to repaint so that `SelectAll` is guaranteed to run with a focused
  // text control (and not be dropped as a no-op).
  let _ = next_frame_ready(&ui_rx, tab_id);

  // SelectAll should render a selection highlight over the text area. The highlight is drawn with
  // a fixed semi-transparent blue, so expect the sampled pixel to become non-black with stable
  // green/blue components.
  ui_tx
    .send(UiToWorker::SelectAll { tab_id })
    .expect("select all");
  let _frame_selected = next_frame_ready_with_probe_predicate(
    &ui_rx,
    tab_id,
    (20, 30),
    "after select all highlight",
    |rgba| {
      rgba[3] == 255 && rgba[0] <= 2 && (40..=45).contains(&rgba[1]) && (73..=78).contains(&rgba[2])
    },
  );

  // Clearing the selection should remove the highlight.
  ui_tx
    .send(UiToWorker::KeyAction {
      tab_id,
      key: KeyAction::ArrowLeft,
    })
    .expect("arrow left");
  let _frame_cleared = next_frame_ready_with_probe_color(
    &ui_rx,
    tab_id,
    (20, 30),
    [0, 0, 0, 255],
    "after clearing selection",
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
