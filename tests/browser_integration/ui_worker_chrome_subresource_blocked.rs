#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;

use super::support::{create_tab_msg, navigate_msg, rgba_at, viewport_changed_msg, DEFAULT_TIMEOUT, TempSite};

#[test]
fn non_about_document_cannot_load_chrome_stylesheet() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; width: 100%; height: 100%; background: rgb(0, 255, 0); }
    </style>
    <link rel="stylesheet" href="chrome://styles/about.css">
  </head>
  <body>
    <div style="height: 100%;"></div>
  </body>
</html>
"#,
  );

  let (ui_tx, ui_rx, join) = spawn_ui_worker("ui_worker_chrome_subresource_blocked")
    .expect("spawn ui worker")
    .split();

  let tab_id = TabId::new();
  ui_tx.send(create_tab_msg(tab_id, None)).expect("create tab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (64, 64), 1.0))
    .expect("viewport");

  ui_tx
    .send(navigate_msg(tab_id, url.clone(), NavigationReason::TypedUrl))
    .expect("navigate");

  super::support::recv_for_tab(&ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::NavigationCommitted { url: committed, .. } if committed == &url)
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationCommitted for {url}"));

  let frame = super::support::recv_for_tab(&ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for {url}"));

  let WorkerToUi::FrameReady { frame, .. } = frame else {
    unreachable!();
  };

  // If the `chrome://` stylesheet load was (incorrectly) allowed, it would override the inline
  // bright-green background. Assert we stayed green, indicating the chrome subresource was blocked.
  let px = rgba_at(&frame.pixmap, 0, 0);
  assert_eq!(
    px,
    [0, 255, 0, 255],
    "expected background to remain green when chrome:// stylesheet is blocked, got rgba={px:?}"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

