#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::worker_loop::spawn_ui_worker;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);

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
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: Default::default(),
    })
    .expect("CreateTab");
  ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (200, 120),
      dpr: 1.0,
    })
    .expect("ViewportChanged");
  ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url,
      reason: NavigationReason::TypedUrl,
    })
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
