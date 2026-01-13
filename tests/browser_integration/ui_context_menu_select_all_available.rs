#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{PointerModifiers, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

#[test]
fn context_menu_advertises_select_all_without_existing_selection() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page_url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; font: 16px/1 sans-serif; }
    </style>
  </head>
  <body>
    Hello world
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-context-menu-select-all").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, Some(page_url.clone())))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (320, 240), 1.0))
    .expect("viewport");

  // Wait for the first paint so the worker has a document loaded.
  let msg = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));
  match msg {
    WorkerToUi::FrameReady { .. } => {}
    WorkerToUi::NavigationFailed { error, .. } => {
      panic!("navigation failed loading {page_url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  let pos_css = (10.0, 10.0);
  ui_tx
    .send(UiToWorker::ContextMenuRequest {
      tab_id,
      pos_css,
      modifiers: PointerModifiers::NONE,
    })
    .expect("send ContextMenuRequest");

  let msg = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ContextMenu { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for ContextMenu for tab {tab_id:?}"));

  match msg {
    WorkerToUi::ContextMenu {
      tab_id: got_tab,
      can_select_all,
      ..
    } => {
      assert_eq!(got_tab, tab_id);
      assert!(
        can_select_all,
        "expected context menu to advertise Select All even without a pre-existing selection"
      );
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  drop(ui_tx);
  join.join().expect("worker join");
}
