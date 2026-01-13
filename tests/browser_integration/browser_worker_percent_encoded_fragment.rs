#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{TabId, UiToWorker, WorkerToUi};
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn next_navigation_committed(rx: &fastrender::ui::WorkerToUiInbox, tab_id: TabId) -> WorkerToUi {
  support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationCommitted { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationCommitted for tab {tab_id:?}"))
}

fn next_frame_ready(
  rx: &fastrender::ui::WorkerToUiInbox,
  tab_id: TabId,
) -> fastrender::ui::messages::RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

#[test]
fn navigation_with_percent_encoded_fragment_scrolls_and_updates_target_pseudoclass() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page_url = site.write(
    "page.html",
    r##"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #top { height: 40px; background: rgb(255, 0, 0); }
            #spacer { height: 2000px; background: rgb(0, 0, 255); }
            #café { height: 100px; background: rgb(255, 0, 0); }
            #café:target { background: rgb(0, 255, 0); }
          </style>
        </head>
        <body>
          <div id="top"></div>
          <div id="spacer"></div>
          <div id="café"></div>
        </body>
      </html>
    "##,
  );
  let url_with_fragment = format!("{page_url}#caf%C3%A9");

  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let tab_id = TabId::new();
  worker
    .tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: Some(url_with_fragment),
      cancel: CancelGens::new(),
    })
    .expect("create tab");
  worker
    .tx
    .send(support::viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("viewport");

  let msg = next_navigation_committed(&worker.rx, tab_id);
  match msg {
    WorkerToUi::NavigationCommitted { url, .. } => {
      assert!(
        url.contains("#caf%C3%A9"),
        "expected committed URL to include the percent-encoded fragment, got {url}"
      );
    }
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  let frame = next_frame_ready(&worker.rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame.pixmap, 10, 10),
    [0, 255, 0, 255],
    "expected first frame to be scrolled to the decoded fragment target with :target styling applied"
  );
  assert!(
    frame.scroll_state.viewport.y > 1000.0,
    "expected initial scroll.y > 1000 after fragment navigation, got {:?}",
    frame.scroll_state.viewport
  );

  drop(worker.tx);
  worker.join.join().expect("worker join");
}
