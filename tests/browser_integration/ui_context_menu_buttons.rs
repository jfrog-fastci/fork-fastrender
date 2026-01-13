#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

#[test]
fn ui_context_menu_buttons_bitmask_includes_secondary_button() {
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
      const link = document.getElementById("link");
      link.addEventListener("contextmenu", function (ev) {
        // Ensure the event reflects a right click, including the `buttons` bitmask.
        if (ev.button === 2 && ev.buttons === 2) {
          ev.preventDefault();
        }
      });
    </script>
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-contextmenu-buttons",
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
      default_prevented,
      ..
    } => {
      assert_eq!(got_tab, tab_id);
      assert!(
        default_prevented,
        "expected JS contextmenu preventDefault() to fire when ev.buttons includes the secondary button bit"
      );
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  };

  drop(ui_tx);
  join.join().expect("worker join");
}
