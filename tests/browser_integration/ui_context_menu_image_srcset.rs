#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  NavigationReason, PointerModifiers, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

// Image hit-testing requires a fully prepared document; keep this generous for slower CI hosts.
const TIMEOUT: Duration = Duration::from_secs(20);

fn fixture() -> (support::TempSite, String, String) {
  let site = support::TempSite::new();

  let _a_url = site.write(
    "a.svg",
    r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"></svg>"#,
  );
  let b_url = site.write(
    "b.svg",
    r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"></svg>"#,
  );

  let index_url = site.write(
    "index.html",
    r#"<!doctype html><html><head><meta charset="utf-8"><style>
html, body { margin: 0; padding: 0; }
img { display: block; width: 64px; height: 64px; }
</style></head><body><img src="a.svg" srcset="a.svg 1x, b.svg 2x"></body></html>"#,
  );

  (site, index_url, b_url)
}

#[test]
fn context_menu_request_uses_selected_srcset_candidate() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_site, index_url, expected_image_url) = fixture();

  let worker =
    spawn_ui_worker("fastr-ui-worker-context-menu-image-srcset").expect("spawn ui worker");
  let tab_id = TabId(1);

  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (320, 240), 2.0))
    .unwrap();
  worker
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      index_url,
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  // Wait for the first paint so the worker has layout artifacts for hit-testing.
  let _frame_msg = support::recv_for_tab(&worker.ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for initial FrameReady for tab {tab_id:?}"));

  let pos_css = (10.0, 10.0);
  worker
    .ui_tx
    .send(UiToWorker::ContextMenuRequest {
      tab_id,
      pos_css,
      modifiers: PointerModifiers::NONE,
    })
    .unwrap();

  let msg = support::recv_for_tab(&worker.ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ContextMenu { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for ContextMenu for tab {tab_id:?}"));

  match msg {
    WorkerToUi::ContextMenu {
      tab_id: got_tab,
      pos_css: got_pos,
      link_url,
      image_url,
      ..
    } => {
      assert_eq!(got_tab, tab_id);
      assert_eq!(got_pos, pos_css);
      assert!(link_url.is_none());
      assert_eq!(image_url.as_deref(), Some(expected_image_url.as_str()));
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  worker.join().unwrap();
}
