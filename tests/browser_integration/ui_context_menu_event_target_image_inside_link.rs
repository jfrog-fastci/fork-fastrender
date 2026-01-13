#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{PointerModifiers, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

#[test]
fn context_menu_event_target_is_deepest_hit_element() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let _svg_url = site.write(
    "a.svg",
    r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"></svg>"#,
  );
  let index_url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      #link { position: absolute; left: 0; top: 0; display: block; }
      #img { display: block; width: 120px; height: 40px; }
    </style>
  </head>
  <body>
    <a id="link" href="target.html"><img id="img" src="a.svg"></a>
    <script>
      document.getElementById("img").addEventListener("contextmenu", function (ev) {
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
    "fastr-ui-worker-contextmenu-event-target-image-inside-link",
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

  // Request a context menu over the image inside the link. The `contextmenu` event should target
  // the `<img>` element (so its listener sees it) and bubble up to the `<a>`.
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
        "expected img contextmenu preventDefault() to be reported via default_prevented=true"
      );
      assert_eq!(link_url.as_deref(), Some(expected_link_url.as_str()));
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  drop(ui_tx);
  join.join().expect("worker join");
}
