#![cfg(feature = "browser_ui")]

use super::support::{self, create_tab_msg, navigate_msg, viewport_changed_msg};
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::worker_loop::spawn_ui_worker;
use std::time::Duration;

// UI worker startup + first paint can take several seconds under load when browser integration
// tests run in parallel (default `cargo test` behavior). Keep this timeout generous to avoid
// flakiness on busy CI hosts.
const TIMEOUT: Duration = Duration::from_secs(15);

#[test]
fn fragment_navigation_scrolls_viewport_to_target() {
  let site = support::TempSite::new();
  let base_url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      #spacer { height: 2000px; }
      #anchor { height: 20px; background: rgb(255, 0, 0); }
    </style>
  </head>
  <body>
    <div id="spacer"></div>
    <div id="anchor">Anchor</div>
  </body>
</html>
"#,
  );
  let url = format!("{base_url}#anchor");

  let handle = spawn_ui_worker("fastr-ui-worker-anchor-scroll").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);

  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let msg = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::ScrollStateUpdated { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .expect("expected ScrollStateUpdated");
  let scroll = match msg {
    WorkerToUi::ScrollStateUpdated { scroll, .. } => scroll,
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  };
  assert!(
    scroll.viewport.y > 0.0,
    "expected fragment navigation to scroll viewport; got {:?}",
    scroll.viewport
  );

  let msg = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("expected FrameReady");
  if let WorkerToUi::FrameReady { frame, .. } = msg {
    assert!(
      frame.scroll_state.viewport.y > 0.0,
      "expected FrameReady to reflect anchor scroll; got {:?}",
      frame.scroll_state.viewport
    );
  }

  drop(ui_tx);
  join.join().expect("join ui worker");
}
