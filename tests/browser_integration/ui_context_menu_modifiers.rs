#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{PointerModifiers, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

#[test]
fn context_menu_request_propagates_modifier_keys_to_js_event() {
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
      #target {
        position: absolute;
        left: 0;
        top: 0;
        width: 120px;
        height: 40px;
        background: rgb(255, 0, 0);
      }
    </style>
  </head>
  <body>
    <div id="target"></div>
    <script>
      document.addEventListener("contextmenu", function (ev) {
        var c = !!ev.ctrlKey;
        var s = !!ev.shiftKey;
        var a = !!ev.altKey;
        var m = !!ev.metaKey;
        // Ensure each modifier flag is forwarded independently (and does not spuriously enable any
        // other modifier flag). We only call preventDefault() when exactly one modifier is set.
        if (s && !c && !a && !m) ev.preventDefault();
        if (c && !s && !a && !m) ev.preventDefault();
        if (a && !c && !s && !m) ev.preventDefault();
        if (m && !c && !s && !a) ev.preventDefault();
        // Also verify combinations (multiple modifier bits set simultaneously).
        if (c && s && !a && !m) ev.preventDefault();
      });
    </script>
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-contextmenu-modifiers",
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

  // Wait for the first paint so the worker has layout artifacts for hit-testing and has executed
  // the inline script that registers the contextmenu listener.
  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {index_url}"));

  let pos_css = (10.0, 10.0);
  let cases: &[(PointerModifiers, bool, &str)] = &[
    (PointerModifiers::SHIFT, true, "shiftKey"),
    (PointerModifiers::CTRL, true, "ctrlKey"),
    (PointerModifiers::ALT, true, "altKey"),
    (PointerModifiers::META, true, "metaKey"),
    (
      PointerModifiers::SHIFT | PointerModifiers::CTRL,
      true,
      "shiftKey+ctrlKey",
    ),
    (PointerModifiers::NONE, false, "no modifiers"),
  ];
  for (modifiers, expect_prevented, label) in cases {
    ui_tx
      .send(UiToWorker::ContextMenuRequest {
        tab_id,
        pos_css,
        modifiers: *modifiers,
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
        ..
      } => {
        assert_eq!(got_tab, tab_id);
        assert_eq!(got_pos, pos_css);
        assert_eq!(
          default_prevented, *expect_prevented,
          "expected default_prevented={expect_prevented} for {label}"
        );
      }
      other => panic!("unexpected WorkerToUi message: {other:?}"),
    }
  }

  drop(ui_tx);
  join.join().expect("worker join");
}
