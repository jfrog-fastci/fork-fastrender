#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

#[test]
fn context_menu_request_on_empty_tab_returns_default_context_menu() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let worker =
    spawn_ui_worker("fastr-ui-worker-context-menu-empty-tab").expect("spawn ui worker");
  let tab_id = TabId::new();

  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("create tab");
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (320, 240), 1.0))
    .expect("viewport");

  let pos_css = (10.0, 10.0);
  worker
    .ui_tx
    .send(UiToWorker::ContextMenuRequest { tab_id, pos_css })
    .expect("context menu request");

  let msg = support::recv_for_tab(&worker.ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ContextMenu { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for ContextMenu for tab {tab_id:?}"));

  match msg {
    WorkerToUi::ContextMenu {
      tab_id: got_tab,
      pos_css: got_pos,
      default_prevented,
      link_url,
      image_url,
      can_copy,
      can_cut,
      can_paste,
      can_select_all,
    } => {
      assert_eq!(got_tab, tab_id);
      assert_eq!(got_pos, pos_css);
      assert!(
        !default_prevented,
        "expected default context menu not to be suppressed on an empty tab"
      );
      assert!(
        link_url.is_none(),
        "expected empty tab to have no link URL, got {link_url:?}"
      );
      assert!(
        image_url.is_none(),
        "expected empty tab to have no image URL, got {image_url:?}"
      );
      assert!(!can_copy, "expected can_copy=false on empty tab");
      assert!(!can_cut, "expected can_cut=false on empty tab");
      assert!(!can_paste, "expected can_paste=false on empty tab");
      assert!(
        !can_select_all,
        "expected can_select_all=false on empty tab"
      );
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  worker.join().expect("worker join");
}

