#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{KeyAction, TabId, UiToWorker, WorkerToUi};
use std::time::Duration;

// Worker startup + first render can take a few seconds under parallel load (CI).
const TIMEOUT: Duration = Duration::from_secs(20);

#[test]
fn browser_thread_file_picker_keyboard_activation_opens() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
        </style>
      </head>
      <body>
        <input type="file" name="f">
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
  match support::recv_until(&rx, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { tab_id: t, .. } if *t == tab_id)
  }) {
    Some(WorkerToUi::FrameReady { .. }) => {}
    Some(other) => panic!("expected FrameReady, got {other:?}"),
    None => panic!("timed out waiting for FrameReady"),
  }

  // Drain initial messages.
  while rx.try_recv().is_ok() {}

  // Focus the input via Tab, then activate it via Space (matching native controls).
  tx.send(UiToWorker::KeyAction {
    tab_id,
    key: KeyAction::Tab,
  })
  .expect("KeyAction Tab");
  tx.send(UiToWorker::KeyAction {
    tab_id,
    key: KeyAction::Space,
  })
  .expect("KeyAction Space");

  support::recv_until(&rx, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FilePickerOpened { tab_id: t, .. } if *t == tab_id)
  })
  .expect("expected FilePickerOpened after keyboard activation");

  tx.send(UiToWorker::FilePickerCancel { tab_id })
    .expect("FilePickerCancel");

  support::recv_until(&rx, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FilePickerClosed { tab_id: t, .. } if *t == tab_id)
  })
  .expect("expected FilePickerClosed after cancel");

  drop(tx);
  drop(rx);
  join.join().unwrap();
}

