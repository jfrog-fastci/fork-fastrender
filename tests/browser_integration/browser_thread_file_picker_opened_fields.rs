#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi};
use std::time::Duration;

// Worker startup + first render can take a few seconds under parallel load (CI).
const TIMEOUT: Duration = Duration::from_secs(20);

#[test]
fn browser_thread_file_picker_opened_includes_multiple_and_accept() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #f1 { position: absolute; left: 0; top: 0; width: 120px; height: 30px; }
          #f2 { position: absolute; left: 0; top: 40px; width: 120px; height: 30px; }
        </style>
      </head>
      <body>
        <input id="f1" type="file" name="f1" accept=".txt, image/*">
        <input id="f2" type="file" name="f2" multiple>
      </body>
    </html>
  "#;
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

  // Click the first file input: it should open a picker with accept metadata.
  let click_f1 = (10.0, 10.0);
  tx.send(UiToWorker::PointerDown {
    tab_id,
    pos_css: click_f1,
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
    click_count: 1,
  })
  .expect("PointerDown f1");
  tx.send(UiToWorker::PointerUp {
    tab_id,
    pos_css: click_f1,
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })
  .expect("PointerUp f1");

  let msg = support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FilePickerOpened { .. })
  })
  .expect("expected FilePickerOpened for f1");

  let WorkerToUi::FilePickerOpened {
    multiple, accept, ..
  } = msg
  else {
    unreachable!("filtered above");
  };
  assert!(!multiple, "f1 should not set multiple");
  assert_eq!(
    accept.as_deref().map(|v| v.trim()),
    Some(".txt, image/*"),
    "f1 should forward accept attribute"
  );

  tx.send(UiToWorker::FilePickerCancel { tab_id })
    .expect("FilePickerCancel f1");
  support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FilePickerClosed { .. })
  })
  .expect("expected FilePickerClosed after cancelling f1 picker");

  // Drain any follow-up messages before interacting with the second input.
  while rx.try_recv().is_ok() {}

  // Click the second file input: it should allow multiple selection and have no accept filter.
  let click_f2 = (10.0, 50.0);
  tx.send(UiToWorker::PointerDown {
    tab_id,
    pos_css: click_f2,
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
    click_count: 1,
  })
  .expect("PointerDown f2");
  tx.send(UiToWorker::PointerUp {
    tab_id,
    pos_css: click_f2,
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })
  .expect("PointerUp f2");

  let msg = support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FilePickerOpened { .. })
  })
  .expect("expected FilePickerOpened for f2");

  let WorkerToUi::FilePickerOpened {
    multiple, accept, ..
  } = msg
  else {
    unreachable!("filtered above");
  };
  assert!(multiple, "f2 should set multiple");
  assert!(accept.is_none(), "f2 should not set accept");

  tx.send(UiToWorker::FilePickerCancel { tab_id })
    .expect("FilePickerCancel f2");
  support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FilePickerClosed { .. })
  })
  .expect("expected FilePickerClosed after cancelling f2 picker");

  drop(tx);
  drop(rx);
  join.join().unwrap();
}

