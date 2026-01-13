#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use std::time::Duration;

fn next_navigation_committed(rx: &fastrender::ui::WorkerToUiInbox, tab_id: TabId) -> WorkerToUi {
  support::recv_for_tab(rx, tab_id, support::DEFAULT_TIMEOUT, |msg| {
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
  let msg = support::recv_for_tab(rx, tab_id, support::DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

#[test]
fn fragment_navigation_updates_target_pseudoclass_even_without_scroll() {
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
            #target { background: rgb(255, 0, 0); height: 20px; }
            #target:target { background: rgb(0, 255, 0); }
          </style>
        </head>
        <body>
          <div id="target"></div>
        </body>
      </html>
    "##,
  );

  let worker = fastrender::ui::spawn_browser_worker_with_factory(support::deterministic_factory())
    .expect("spawn browser worker");
  let tab_id = TabId::new();
  worker
    .tx
    .send(support::create_tab_msg(tab_id, Some(page_url.clone())))
    .expect("create tab");
  worker
    .tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .expect("viewport");

  let first_frame = next_frame_ready(&worker.rx, tab_id);
  assert!(
    first_frame.scroll_state.viewport.y.abs() < 0.1,
    "expected initial viewport scroll to be at top, got {:?}",
    first_frame.scroll_state.viewport
  );
  assert_eq!(
    support::rgba_at(&first_frame.pixmap, 10, 10),
    [255, 0, 0, 255],
    "expected initial frame to render #target without :target styling"
  );

  // Drain any follow-up messages from the initial navigation.
  let _ = support::drain_for(&worker.rx, Duration::from_millis(50));

  let url_with_fragment = format!("{page_url}#target");
  worker
    .tx
    .send(support::navigate_msg(
      tab_id,
      url_with_fragment.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate");

  let msg = next_navigation_committed(&worker.rx, tab_id);
  match msg {
    WorkerToUi::NavigationCommitted { url, .. } => {
      assert!(
        url.contains("#target"),
        "expected committed URL to include #target, got {url}"
      );
    }
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  let frame = next_frame_ready(&worker.rx, tab_id);
  assert!(
    frame.scroll_state.viewport.y.abs() < 0.1,
    "expected fragment navigation not to scroll, got {:?}",
    frame.scroll_state.viewport
  );
  assert_eq!(
    support::rgba_at(&frame.pixmap, 10, 10),
    [0, 255, 0, 255],
    "expected :target styling to update after fragment navigation"
  );

  drop(worker.tx);
  worker.join.join().expect("worker join");
}
