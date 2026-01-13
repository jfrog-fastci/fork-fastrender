#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{PointerModifiers, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

#[test]
fn context_menu_prevent_default_still_sends_suppressed_response() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let index_url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      /* Give the link a predictable hit target so contextmenu dispatch lands on the <a>. */
      #link {
        position: absolute;
        left: 0;
        top: 0;
        width: 120px;
        height: 40px;
        display: block;
        background: rgb(255, 0, 0);
      }
    </style>
  </head>
  <body>
    <a id="link" href="target.html">Link</a>
    <script>
      document.getElementById("link").addEventListener("contextmenu", function (ev) {
        ev.preventDefault();
      });
    </script>
  </body>
</html>
"#,
  );
  let _target_url = site.write(
    "target.html",
    r#"<!doctype html><html><head><meta charset="utf-8"></head><body>Target</body></html>"#,
  );

  let expected_link_url = url::Url::parse(&index_url)
    .expect("parse base URL")
    .join("target.html")
    .expect("join relative href")
    .to_string();

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-contextmenu-prevent-default",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, Some(index_url.clone())))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("viewport");

  // Wait for the first paint so the worker has layout artifacts for hit-testing.
  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {index_url}"));

  // Request a context menu at the link target. JS `preventDefault()` should not suppress the
  // protocol response; instead, the worker reports `default_prevented=true`.
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
      pos_css: got_pos,
      default_prevented,
      link_url,
      ..
    } => {
      assert_eq!(got_tab, tab_id);
      assert_eq!(got_pos, pos_css);
      assert!(
        default_prevented,
        "expected contextmenu preventDefault() to be reported via default_prevented=true"
      );
      assert_eq!(link_url.as_deref(), Some(expected_link_url.as_str()));
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn context_menu_prevent_default_still_sends_suppressed_response_without_id() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let index_url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      /* Give the link a predictable hit target so contextmenu dispatch lands on the <a>. */
      a {
        position: absolute;
        left: 0;
        top: 0;
        width: 120px;
        height: 40px;
        display: block;
        background: rgb(255, 0, 0);
      }
    </style>
  </head>
  <body>
    <a href="target.html">Link</a>
    <script>
      document.querySelector("a").addEventListener("contextmenu", function (ev) {
        ev.preventDefault();
      });
    </script>
  </body>
</html>
"#,
  );
  let _target_url = site.write(
    "target.html",
    r#"<!doctype html><html><head><meta charset="utf-8"></head><body>Target</body></html>"#,
  );

  let expected_link_url = url::Url::parse(&index_url)
    .expect("parse base URL")
    .join("target.html")
    .expect("join relative href")
    .to_string();

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-contextmenu-prevent-default-without-id",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, Some(index_url.clone())))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("viewport");

  // Wait for the first paint so the worker has layout artifacts for hit-testing.
  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {index_url}"));

  // Request a context menu at the link target. JS `preventDefault()` should not suppress the
  // protocol response; instead, the worker reports `default_prevented=true`.
  let pos_css = (10.0, 10.0);
  ui_tx
    .send(UiToWorker::ContextMenuRequest { tab_id, pos_css })
    .expect("send ContextMenuRequest");

  let msg = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
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
        default_prevented,
        "expected contextmenu preventDefault() to be reported via default_prevented=true"
      );
      assert_eq!(link_url.as_deref(), Some(expected_link_url.as_str()));
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  drop(ui_tx);
  join.join().expect("worker join");
}
