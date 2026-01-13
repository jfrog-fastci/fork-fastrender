#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

#[test]
fn ui_context_menu_can_select_all_without_selection() {
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
      p { margin: 0; padding: 0; font: 40px/80px monospace; user-select: text; }
    </style>
  </head>
  <body>
    <p>hello</p>
  </body>
</html>
"#,
  );

  let handle =
    spawn_ui_worker("fastr-ui-worker-context-menu-can-select-all-without-selection")
      .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, Some(url)))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (320, 160), 1.0))
    .expect("viewport");

  // Wait for the first paint so the worker has layout artifacts for hit-testing.
  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  // Request a context menu on page text without creating any selection.
  let pos_css = (10.0, 40.0);
  ui_tx
    .send(UiToWorker::ContextMenuRequest { tab_id, pos_css })
    .expect("send ContextMenuRequest");

  let msg = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ContextMenu { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for ContextMenu for tab {tab_id:?}"));

  match msg {
    WorkerToUi::ContextMenu {
      can_select_all,
      can_copy,
      can_cut,
      can_paste,
      ..
    } => {
      assert!(can_select_all, "expected page context menu to allow Select All");
      assert!(!can_copy, "expected Copy to be disabled without a selection");
      assert!(!can_cut, "expected Cut to be disabled outside text controls");
      assert!(!can_paste, "expected Paste to be disabled outside text controls");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  drop(ui_tx);
  join.join().expect("worker join");
}

