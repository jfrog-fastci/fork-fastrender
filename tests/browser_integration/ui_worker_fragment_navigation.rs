#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, PointerButton, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::worker::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn next_navigation_committed(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> WorkerToUi {
  support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationCommitted { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationCommitted for tab {tab_id:?}"))
}

fn next_frame_ready(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> fastrender::ui::messages::RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| matches!(msg, WorkerToUi::FrameReady { .. }))
    .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));
  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

#[test]
fn navigation_with_fragment_scrolls_to_target_before_first_frame() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page_url = site.write(
    "page.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            .spacer { height: 2000px; }
            #target { height: 20px; background: rgb(255, 0, 0); }
            #target:target { background: rgb(0, 255, 0); }
          </style>
        </head>
        <body>
          <div class="spacer"></div>
          <div id="target"></div>
          <div class="spacer"></div>
        </body>
      </html>
    "#,
  );
  let url = format!("{page_url}#target");

  let worker = spawn_ui_worker("fastr-ui-worker-fragment-initial").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 120), 1.0))
    .unwrap();
  worker
    .ui_tx
    .send(support::navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .unwrap();

  let msg = next_navigation_committed(&worker.ui_rx, tab_id);
  match msg {
    WorkerToUi::NavigationCommitted { url, .. } => {
      assert!(url.contains("#target"), "expected committed URL to include #target, got {url}");
    }
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  assert!(
    frame.scroll_state.viewport.y > 0.0,
    "expected first frame to be scrolled for fragment navigation, got {:?}",
    frame.scroll_state.viewport
  );
  assert_eq!(
    support::rgba_at(&frame.pixmap, 10, 10),
    [0, 255, 0, 255],
    "expected :target styling + scroll to bring the green target into view"
  );

  worker.join().unwrap();
}

#[test]
fn navigation_with_percent_encoded_fragment_scrolls_to_target_before_first_frame() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page_url = site.write(
    "page.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            .spacer { height: 2000px; }
            div[id="foo#bar"] { height: 20px; background: rgb(255, 0, 0); }
            div[id="foo#bar"]:target { background: rgb(0, 255, 0); }
          </style>
        </head>
        <body>
          <div class="spacer"></div>
          <div id="foo#bar"></div>
          <div class="spacer"></div>
        </body>
      </html>
    "#,
  );
  let url = format!("{page_url}#foo%23bar");

  let worker = spawn_ui_worker("fastr-ui-worker-fragment-percent").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 120), 1.0))
    .unwrap();
  worker
    .ui_tx
    .send(support::navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .unwrap();

  let msg = next_navigation_committed(&worker.ui_rx, tab_id);
  match msg {
    WorkerToUi::NavigationCommitted { url, .. } => {
      assert!(
        url.contains("#foo%23bar"),
        "expected committed URL to include #foo%23bar, got {url}"
      );
    }
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  assert!(
    frame.scroll_state.viewport.y > 0.0,
    "expected first frame to be scrolled for fragment navigation, got {:?}",
    frame.scroll_state.viewport
  );
  assert_eq!(
    support::rgba_at(&frame.pixmap, 10, 10),
    [0, 255, 0, 255],
    "expected :target styling to match decoded fragment after scrolling"
  );

  worker.join().unwrap();
}

#[test]
fn same_document_fragment_click_updates_url_and_scrolls_without_reload() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "page.html",
    r##"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #link { display: block; width: 120px; height: 40px; background: rgb(255, 0, 0); }
            .spacer { height: 2000px; }
            #target { height: 20px; background: rgb(255, 0, 0); }
            #target:target { background: rgb(0, 255, 0); }
          </style>
        </head>
        <body>
          <a href="#target" id="link">Go</a>
          <div class="spacer"></div>
          <div id="target"></div>
          <div class="spacer"></div>
        </body>
      </html>
    "##,
  );

  let worker = spawn_ui_worker("fastr-ui-worker-fragment-same-doc").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("create tab");
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("viewport");
  worker
    .ui_tx
    .send(support::navigate_msg(tab_id, url.clone(), NavigationReason::TypedUrl))
    .expect("navigate");

  // Wait for an initial frame so hit-testing has a layout cache.
  let initial_frame = next_frame_ready(&worker.ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&initial_frame.pixmap, 10, 10),
    [255, 0, 0, 255],
    "expected link to render at top before fragment navigation"
  );

  // Drain any follow-up messages from the initial navigation.
  let _ = support::drain_for(&worker.ui_rx, Duration::from_millis(50));

  // Click the link at the top-left of the page.
  worker
    .ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .expect("pointer down");
  worker
    .ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .expect("pointer up");

  let deadline = Instant::now() + TIMEOUT;
  let mut saw_failed = false;
  let mut committed_url = None::<String>;
  let mut committed_can_go_back = None::<bool>;
  let mut scroll_y = None::<f32>;
  let mut final_pixel = None::<[u8; 4]>;
  let mut captured: Vec<WorkerToUi> = Vec::new();

  while Instant::now() < deadline {
    match worker.ui_rx.recv_timeout(Duration::from_millis(200)) {
      Ok(msg) => {
        match &msg {
          WorkerToUi::NavigationFailed { tab_id: got, .. } if *got == tab_id => {
            saw_failed = true;
          }
          WorkerToUi::NavigationCommitted {
            tab_id: got,
            url,
            can_go_back,
            ..
          } if *got == tab_id => {
            if url.contains("#target") {
              committed_url = Some(url.clone());
              committed_can_go_back = Some(*can_go_back);
            }
          }
          WorkerToUi::ScrollStateUpdated { tab_id: got, scroll } if *got == tab_id => {
            if committed_url.is_some() {
              scroll_y = Some(scroll.viewport.y);
            }
          }
          WorkerToUi::FrameReady { tab_id: got, frame } if *got == tab_id => {
            if committed_url.is_some() {
              final_pixel = Some(support::rgba_at(&frame.pixmap, 10, 10));
            }
          }
          _ => {}
        }
        captured.push(msg);

        if saw_failed {
          break;
        }
        if committed_url.is_some() && scroll_y.is_some() && final_pixel.is_some() {
          break;
        }
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  if committed_url.is_none() || scroll_y.is_none() || final_pixel.is_none() {
    // Drain for a moment to provide better assertion errors.
    captured.extend(support::drain_for(&worker.ui_rx, Duration::from_millis(200)));
  }

  assert!(
    !saw_failed,
    "did not expect NavigationFailed; messages:\n{}",
    support::format_messages(&captured)
  );

  let committed_url = committed_url.unwrap_or_default();
  assert!(
    committed_url.contains("#target"),
    "expected NavigationCommitted with #target, got {committed_url}; messages:\n{}",
    support::format_messages(&captured)
  );

  // Same-document fragment navigations should create a history entry (allowing Back).
  assert_eq!(
    committed_can_go_back,
    Some(true),
    "expected can_go_back=true after fragment navigation, messages:\n{}",
    support::format_messages(&captured)
  );

  let scroll_y = scroll_y.unwrap_or(0.0);
  assert!(
    scroll_y > 1000.0,
    "expected viewport scroll y to increase after fragment navigation, got {scroll_y}; messages:\n{}",
    support::format_messages(&captured)
  );
  assert_eq!(
    final_pixel,
    Some([0, 255, 0, 255]),
    "expected :target styling to update after fragment click; messages:\n{}",
    support::format_messages(&captured)
  );

  worker.join().expect("worker join");
}

#[test]
fn fragment_navigation_pushes_history_and_back_restores_previous_scroll() {
  let _lock = super::stage_listener_test_lock();
  let site = support::TempSite::new();
  let page_url = site.write(
    "page.html",
    r##"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            /* Place the hash link at y=150 so that after we scroll by 150px it sits at the top of the viewport. */
            #pre { height: 150px; }
            #link { display: block; width: 120px; height: 40px; background: rgb(255, 0, 0); }
            .spacer { height: 2000px; }
            #target { height: 20px; background: rgb(0, 255, 0); }
          </style>
        </head>
        <body>
          <div id="pre"></div>
          <a href="#target" id="link">Go</a>
          <div class="spacer"></div>
          <div id="target">target</div>
        </body>
      </html>
    "##,
  );

  let worker = spawn_ui_worker("fastr-ui-worker-fragment-back").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 120), 1.0))
    .unwrap();
  worker
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      page_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  // Wait for the initial frame so the worker has cached layout artifacts.
  support::recv_for_tab(&worker.ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("expected initial FrameReady");
  let _ = support::drain_for(&worker.ui_rx, Duration::from_millis(50));

  // Scroll down some so the "pre-fragment" history entry has a non-zero scroll position.
  worker
    .ui_tx
    .send(support::scroll_msg(tab_id, (0.0, 150.0), None))
    .unwrap();

  let msg = support::recv_for_tab(&worker.ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ScrollStateUpdated { .. })
  })
  .expect("expected ScrollStateUpdated after scroll");
  let WorkerToUi::ScrollStateUpdated { scroll, .. } = msg else {
    unreachable!();
  };
  let scroll_before = scroll.viewport.y;
  assert!(
    scroll_before > 0.0,
    "expected pre-fragment scroll to be > 0, got {scroll_before}"
  );
  let _ = support::drain_for(&worker.ui_rx, Duration::from_millis(50));

  // Click the fixed-position link to jump to the fragment target.
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

  support::recv_for_tab(&worker.ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationCommitted { url, .. } if url.ends_with("#target")
    )
  })
  .expect("expected NavigationCommitted for fragment navigation");
  let msg = support::recv_for_tab(&worker.ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ScrollStateUpdated { .. })
  })
  .expect("expected ScrollStateUpdated for fragment navigation");
  let WorkerToUi::ScrollStateUpdated { scroll, .. } = msg else {
    unreachable!();
  };
  let scroll_after = scroll.viewport.y;
  assert!(
    scroll_after > scroll_before,
    "expected fragment navigation to increase scroll y (before={scroll_before}, after={scroll_after})"
  );
  let _ = support::drain_for(&worker.ui_rx, Duration::from_millis(50));

  // Back should restore the scroll position from before the fragment navigation.
  worker.ui_tx.send(UiToWorker::GoBack { tab_id }).unwrap();
  support::recv_for_tab(&worker.ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationCommitted { url, .. } if url == &page_url
    )
  })
  .expect("expected NavigationCommitted after going back");

  let msg = support::recv_for_tab(&worker.ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ScrollStateUpdated { .. })
  })
  .expect("expected ScrollStateUpdated after going back");
  let WorkerToUi::ScrollStateUpdated { scroll, .. } = msg else {
    unreachable!();
  };
  assert_eq!(
    scroll.viewport.y, scroll_before,
    "expected back to restore previous scroll position"
  );

  worker.join().unwrap();
}
