#![cfg(feature = "browser_ui")]

use crate::browser_integration::support::{drain_for, recv_for_tab, TempSite};
use fastrender::ui::messages::{NavigationReason, PointerButton, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::worker::spawn_ui_worker;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

#[test]
fn navigation_with_fragment_scrolls_to_target_before_first_frame() {
  let _lock = super::stage_listener_test_lock();
  let site = TempSite::new();
  let page_url = site.write(
    "page.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            .spacer { height: 2000px; }
            #target { height: 20px; background: rgb(0, 255, 0); }
          </style>
        </head>
        <body>
          <div class="spacer"></div>
          <div id="target">target</div>
        </body>
      </html>
    "#,
  );
  let url = format!("{page_url}#target");

  let worker = spawn_ui_worker("fastr-ui-worker-fragment-initial").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: Default::default(),
    })
    .unwrap();
  worker
    .ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (200, 120),
      dpr: 1.0,
    })
    .unwrap();
  worker
    .ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url,
      reason: NavigationReason::TypedUrl,
    })
    .unwrap();

  let msg = recv_for_tab(&worker.ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ScrollStateUpdated { .. })
  })
  .expect("expected ScrollStateUpdated");
  let WorkerToUi::ScrollStateUpdated { scroll, .. } = msg else {
    unreachable!();
  };
  assert!(
    scroll.viewport.y > 0.0,
    "expected initial scroll.y > 0 after fragment navigation, got {:?}",
    scroll.viewport
  );

  worker.join().unwrap();
}

#[test]
fn same_document_fragment_click_scrolls_without_full_navigation() {
  let _lock = super::stage_listener_test_lock();
  let site = TempSite::new();
  let page_url = site.write(
    "page.html",
    r##"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #link { display: block; width: 100px; height: 40px; background: rgb(255, 0, 0); }
            .spacer { height: 2000px; }
            #target { height: 20px; background: rgb(0, 255, 0); }
          </style>
        </head>
        <body>
          <a href="#target" id="link">Go</a>
          <div class="spacer"></div>
          <div id="target">target</div>
        </body>
      </html>
    "##,
  );

  let worker = spawn_ui_worker("fastr-ui-worker-fragment-same-doc").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: Default::default(),
    })
    .unwrap();
  worker
    .ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (200, 120),
      dpr: 1.0,
    })
    .unwrap();
  worker
    .ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: page_url,
      reason: NavigationReason::TypedUrl,
    })
    .unwrap();

  // Wait for the initial frame so the worker has cached layout artifacts for hit-testing.
  recv_for_tab(&worker.ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("expected initial FrameReady");

  // Drain any follow-up messages from the initial navigation so assertions below are scoped to the
  // fragment click.
  let _ = drain_for(&worker.ui_rx, Duration::from_millis(50));

  // Click the link at the top-left of the page.
  worker
    .ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .unwrap();
  worker
    .ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .unwrap();

  let msgs = drain_for(&worker.ui_rx, TIMEOUT);

  assert!(
    !msgs.iter().any(|msg| matches!(msg, WorkerToUi::NavigationStarted { .. }))
      && !msgs
        .iter()
        .any(|msg| matches!(msg, WorkerToUi::NavigationCommitted { .. })),
    "expected no full navigation messages for fragment click, got:\n{}",
    crate::browser_integration::support::format_messages(&msgs)
  );

  let scrolled = msgs.iter().any(|msg| match msg {
    WorkerToUi::ScrollStateUpdated { scroll, .. } => scroll.viewport.y > 0.0,
    _ => false,
  });
  assert!(
    scrolled,
    "expected ScrollStateUpdated with viewport.y > 0 after fragment click, got:\n{}",
    crate::browser_integration::support::format_messages(&msgs)
  );

  let saw_frame = msgs.iter().any(|msg| match msg {
    WorkerToUi::FrameReady { frame, .. } => frame.scroll_state.viewport.y > 0.0,
    _ => false,
  });
  assert!(
    saw_frame,
    "expected FrameReady with scroll_state.viewport.y > 0 after fragment click, got:\n{}",
    crate::browser_integration::support::format_messages(&msgs)
  );

  worker.join().unwrap();
}
