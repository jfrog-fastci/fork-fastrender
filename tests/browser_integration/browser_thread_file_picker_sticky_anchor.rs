#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{KeyAction, TabId, UiToWorker, WorkerToUi};
use std::time::Duration;

// Worker startup + first render can take a few seconds under parallel load (CI).
const TIMEOUT: Duration = Duration::from_secs(20);

#[test]
fn browser_thread_file_picker_anchor_accounts_for_sticky_positioning() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #spacer { height: 200px; }
          #header {
            position: sticky;
            top: 0;
            height: 30px;
            background: #eee;
          }
          #file {
            position: absolute;
            left: 0;
            top: 0;
            width: 120px;
            height: 20px;
          }
          #below { height: 2000px; }
        </style>
      </head>
      <body>
        <div id="spacer"></div>
        <div id="header"><input id="file" type="file" autofocus></div>
        <div id="below"></div>
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

  // Wait for the first rendered frame so the tab has a live document and the autofocus target is
  // focused.
  support::recv_until(&rx, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { tab_id: t, .. } if *t == tab_id)
  })
  .expect("FrameReady");

  // Drain initial messages so subsequent waits see only relevant updates.
  while rx.try_recv().is_ok() {}

  // Scroll far enough that the sticky header should be pinned to the top of the viewport.
  tx.send(UiToWorker::ScrollTo {
    tab_id,
    pos_css: (0.0, 250.0),
  })
  .expect("ScrollTo");

  support::recv_until(&rx, TIMEOUT, |msg| match msg {
    WorkerToUi::ScrollStateUpdated { tab_id: t, scroll }
      if *t == tab_id && (scroll.viewport.y - 250.0).abs() < 2.0 =>
    {
      true
    }
    _ => false,
  })
  .expect("ScrollStateUpdated after ScrollTo");

  // Drain any follow-up paint messages.
  while rx.try_recv().is_ok() {}

  // Activate the focused file input (Space is treated like a click for file inputs).
  tx.send(UiToWorker::KeyAction {
    tab_id,
    key: KeyAction::Space,
  })
  .expect("KeyAction Space");

  let msg = support::recv_until(&rx, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FilePickerOpened { tab_id: t, .. } if *t == tab_id)
  })
  .expect("FilePickerOpened");

  let WorkerToUi::FilePickerOpened { anchor_css, .. } = msg else {
    unreachable!();
  };

  assert!(
    anchor_css.y() >= -1.0 && anchor_css.y() < 30.0,
    "expected sticky file input anchor to be near the top of the viewport, got {anchor_css:?}"
  );

  tx.send(UiToWorker::FilePickerCancel { tab_id })
    .expect("FilePickerCancel");
  support::recv_until(&rx, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FilePickerClosed { tab_id: t } if *t == tab_id)
  })
  .expect("FilePickerClosed");

  drop(tx);
  drop(rx);
  join.join().unwrap();
}

