#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

// Link hit-testing requires a fully prepared document, so keep a generous timeout to accommodate
// renderer initialization and layout work on slower CI hosts.
const TIMEOUT: Duration = Duration::from_secs(20);

fn fixture() -> (support::TempSite, String, String) {
  let site = support::TempSite::new();
  let index_url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      a { display: block; padding: 40px; font: 16px/1 sans-serif; }
    </style>
  </head>
  <body>
    <a href="target.html#frag">Link</a>
  </body>
</html>
"#,
  );
  let _target_url = site.write(
    "target.html",
    r#"<!doctype html><html><head><meta charset="utf-8"></head><body>Target</body></html>"#,
  );

  let expected = url::Url::parse(&index_url)
    .expect("parse base URL")
    .join("target.html#frag")
    .expect("join relative href")
    .to_string();

  (site, index_url, expected)
}

#[test]
fn context_menu_request_resolves_link_url() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let (_site, index_url, expected_link_url) = fixture();

  let worker = spawn_ui_worker("fastr-ui-worker-context-menu").expect("spawn ui worker");
  let tab_id = TabId(1);

  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (320, 240), 1.0))
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
    .send(UiToWorker::ContextMenuRequest { tab_id, pos_css })
    .unwrap();

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
      ..
    } => {
      assert_eq!(got_tab, tab_id);
      assert_eq!(got_pos, pos_css);
      assert!(
        !default_prevented,
        "expected default context menu not to be suppressed"
      );
      assert_eq!(link_url.as_deref(), Some(expected_link_url.as_str()));
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  worker.join().unwrap();
}
