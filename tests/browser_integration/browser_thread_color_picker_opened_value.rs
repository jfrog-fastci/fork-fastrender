#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi};
use std::time::Duration;

// Worker startup + first render can take a few seconds under parallel load (CI).
const TIMEOUT: Duration = Duration::from_secs(20);

#[test]
fn browser_thread_color_picker_opened_value_is_sanitized_from_input_value_attribute() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let html = r##"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #valid { position: absolute; left: 0; top: 0; width: 120px; height: 30px; }
          #invalid { position: absolute; left: 0; top: 40px; width: 120px; height: 30px; }
        </style>
      </head>
      <body>
        <input id=valid type=color value="#00ff00">
        <input id=invalid type=color value="red">
      </body>
    </html>
  "##;
  let url = site.write("page.html", html);

  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let fastrender::ui::BrowserWorkerHandle { tx, rx, join } = worker;

  let tab_id = TabId::new();
  tx.send(support::create_tab_msg_with_cancel(
    tab_id,
    Some(url),
    CancelGens::new(),
  ))
  .expect("CreateTab");
  tx.send(UiToWorker::SetActiveTab { tab_id })
    .expect("SetActiveTab");
  tx.send(support::viewport_changed_msg(tab_id, (240, 120), 1.0))
    .expect("ViewportChanged");

  // Wait for the first rendered frame so the tab has a live document.
  match support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  }) {
    Some(WorkerToUi::FrameReady { .. }) => {}
    Some(other) => panic!("expected FrameReady, got {other:?}"),
    None => panic!("timed out waiting for FrameReady"),
  }

  // Drain initial messages.
  while rx.try_recv().is_ok() {}

  let click = |pos_css: (f32, f32)| {
    tx.send(UiToWorker::PointerDown {
      tab_id,
      pos_css,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("PointerDown");
    tx.send(UiToWorker::PointerUp {
      tab_id,
      pos_css,
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("PointerUp");
  };

  // Click the valid `value=` input.
  click((10.0, 10.0));
  let msg = support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ColorPickerOpened { .. })
  })
  .expect("expected ColorPickerOpened for valid color input");
  let WorkerToUi::ColorPickerOpened { value, .. } = msg else {
    unreachable!("filtered above");
  };
  assert_eq!(value, "#00ff00");

  tx.send(UiToWorker::ColorPickerCancel { tab_id })
    .expect("ColorPickerCancel");
  support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ColorPickerClosed { .. })
  })
  .expect("expected ColorPickerClosed after cancel");

  // Drain any repaint/input messages before opening the next picker.
  while rx.try_recv().is_ok() {}

  // Click the invalid `value=` input: it should sanitize to black.
  click((10.0, 50.0));
  let msg = support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ColorPickerOpened { .. })
  })
  .expect("expected ColorPickerOpened for invalid color input");
  let WorkerToUi::ColorPickerOpened { value, .. } = msg else {
    unreachable!("filtered above");
  };
  assert_eq!(value, "#000000");

  tx.send(UiToWorker::ColorPickerCancel { tab_id })
    .expect("ColorPickerCancel (invalid)");
  support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ColorPickerClosed { .. })
  })
  .expect("expected ColorPickerClosed after cancel (invalid)");

  drop(tx);
  drop(rx);
  join.join().unwrap();
}
