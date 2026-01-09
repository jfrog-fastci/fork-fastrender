#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{PointerButton, TabId, UiToWorker, WorkerToUi};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn next_navigation_committed(
  rx: &std::sync::mpsc::Receiver<WorkerToUi>,
  tab_id: TabId,
) -> WorkerToUi {
  support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationCommitted { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationCommitted for tab {tab_id:?}"))
}

fn next_frame_ready(
  rx: &std::sync::mpsc::Receiver<WorkerToUi>,
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
fn navigation_with_fragment_scrolls_to_target_before_first_frame() {
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
            #target { height: 100px; background: rgb(0, 255, 0); }
          </style>
        </head>
        <body>
          <div id="top"></div>
          <div id="spacer"></div>
          <div id="target"></div>
        </body>
      </html>
    "##,
  );
  let url = format!("{page_url}#target");

  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let tab_id = TabId::new();
  worker
    .tx
    .send(support::create_tab_msg(tab_id, Some(url)))
    .expect("create tab");
  worker
    .tx
    .send(support::viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("viewport");

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
  assert_eq!(
    support::rgba_at(&frame.pixmap, 10, 10),
    [0, 255, 0, 255],
    "expected first frame to be scrolled to the target element"
  );
  assert!(
    frame.scroll_state.viewport.y > 0.0,
    "expected initial scroll.y > 0 after fragment navigation, got {:?}",
    frame.scroll_state.viewport
  );

  drop(worker.tx);
  worker.join.join().expect("worker join");
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
            #spacer { height: 2000px; background: rgb(0, 0, 255); }
            #target { height: 100px; background: rgb(0, 255, 0); }
          </style>
        </head>
        <body>
          <a href="#target" id="link">Go</a>
          <div id="spacer"></div>
          <div id="target"></div>
        </body>
      </html>
    "##,
  );

  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let tab_id = TabId::new();
  worker
    .tx
    .send(support::create_tab_msg(tab_id, Some(url.clone())))
    .expect("create tab");
  worker
    .tx
    .send(support::viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("viewport");

  // Wait for an initial frame so hit-testing has a layout cache.
  let initial_frame = next_frame_ready(&worker.rx, tab_id);
  assert_eq!(
    support::rgba_at(&initial_frame.pixmap, 10, 10),
    [255, 0, 0, 255],
    "expected link to render at top before fragment navigation"
  );

  // Drain any follow-up messages from the initial navigation.
  let _ = support::drain_for(&worker.rx, Duration::from_millis(50));

  worker
    .tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .expect("pointer down");
  worker
    .tx
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
    match worker.rx.recv_timeout(Duration::from_millis(200)) {
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
              scroll_y.get_or_insert(frame.scroll_state.viewport.y);
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
    captured.extend(support::drain_for(&worker.rx, Duration::from_millis(200)));
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
    "expected top pixel to show the #target background after scrolling; messages:\n{}",
    support::format_messages(&captured)
  );

  // Back navigation should restore the pre-fragment viewport position and URL without reloading.
  let _ = support::drain_for(&worker.rx, Duration::from_millis(50));
  worker
    .tx
    .send(UiToWorker::GoBack { tab_id })
    .expect("go back");
  let msg = next_navigation_committed(&worker.rx, tab_id);
  let (back_url, can_go_back, can_go_forward) = match msg {
    WorkerToUi::NavigationCommitted {
      url,
      can_go_back,
      can_go_forward,
      ..
    } => (url, can_go_back, can_go_forward),
    WorkerToUi::NavigationFailed { url, error, .. } => panic!("back navigation failed for {url}: {error}"),
    other => panic!("unexpected WorkerToUi message after back: {other:?}"),
  };
  assert_eq!(back_url, url, "expected back navigation URL to match initial page");
  assert!(
    !can_go_back && can_go_forward,
    "expected can_go_back=false and can_go_forward=true after back, got back={can_go_back} forward={can_go_forward}"
  );
  let back_frame = next_frame_ready(&worker.rx, tab_id);
  assert!(
    back_frame.scroll_state.viewport.y.abs() < 1.0,
    "expected viewport scroll to return to top after back, got {:?}",
    back_frame.scroll_state.viewport
  );
  assert_eq!(
    support::rgba_at(&back_frame.pixmap, 10, 10),
    [255, 0, 0, 255],
    "expected top pixel to show the link background after back navigation"
  );

  drop(worker.tx);
  worker.join.join().expect("worker join");
}
