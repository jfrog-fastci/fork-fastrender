#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::worker_loop::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn recv_until<T>(
  rx: &Receiver<WorkerToUi>,
  timeout: Duration,
  mut f: impl FnMut(WorkerToUi) -> Option<T>,
) -> T {
  let deadline = Instant::now() + timeout;
  loop {
    let now = Instant::now();
    let remaining = deadline
      .checked_duration_since(now)
      .unwrap_or(Duration::from_secs(0));
    assert!(
      remaining > Duration::from_secs(0),
      "timed out waiting for expected WorkerToUi message"
    );

    let msg = rx
      .recv_timeout(remaining)
      .unwrap_or_else(|err| panic!("timed out waiting for WorkerToUi message: {err}"));
    if let Some(value) = f(msg) {
      return value;
    }
  }
}

fn wait_for_frame_ready(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> fastrender::ui::messages::RenderedFrame {
  recv_until(rx, Duration::from_secs(10), |msg| match msg {
    WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => Some(frame),
    _ => None,
  })
}

fn wait_for_scroll_update(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> fastrender::scroll::ScrollState {
  recv_until(rx, Duration::from_secs(10), |msg| match msg {
    WorkerToUi::ScrollStateUpdated { tab_id: got, scroll } if got == tab_id => Some(scroll),
    _ => None,
  })
}

fn make_test_page() -> (tempfile::TempDir, String) {
  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #scroller {
            width: 120px;
            height: 60px;
            overflow-y: scroll;
            border: 1px solid black;
          }
          #scroller > .content {
            height: 400px;
            background: linear-gradient(#eee, #ccc);
          }
          .spacer { height: 2000px; }
        </style>
      </head>
      <body>
        <div id="scroller"><div class="content"></div></div>
        <div class="spacer"></div>
      </body>
    </html>
  "#;

  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = format!("file://{}/index.html", dir.path().display());
  (dir, url)
}

#[test]
fn scroll_without_pointer_updates_viewport_scroll() {
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_test_page();

  let handle = spawn_ui_worker("fastr-ui-worker-scroll-without-pointer").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);
  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
    })
    .expect("CreateTab");
  ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (200, 100),
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

  let frame = wait_for_frame_ready(&ui_rx, tab_id);
  let initial_scroll = frame.scroll_state.viewport;

  ui_tx
    .send(UiToWorker::Scroll {
      tab_id,
      delta_css: (0.0, 40.0),
      pointer_css: None,
    })
    .expect("Scroll");

  let updated = wait_for_scroll_update(&ui_rx, tab_id);
  let frame = wait_for_frame_ready(&ui_rx, tab_id);

  assert!(
    (updated.viewport.y - (initial_scroll.y + 40.0)).abs() < 1e-3,
    "expected viewport y scroll to increase by 40, was {:?} then {:?}",
    initial_scroll,
    updated.viewport
  );
  assert_eq!(
    frame.scroll_state, updated,
    "FrameReady.scroll_state should match ScrollStateUpdated"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn scroll_with_pointer_updates_element_scroll_offsets() {
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_test_page();

  let handle = spawn_ui_worker("fastr-ui-worker-scroll-with-pointer").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);
  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
    })
    .expect("CreateTab");
  ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (200, 100),
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

  let _ = wait_for_frame_ready(&ui_rx, tab_id);

  // Inside the #scroller element (it starts at the top of the page with margin: 0).
  ui_tx
    .send(UiToWorker::Scroll {
      tab_id,
      delta_css: (0.0, 60.0),
      pointer_css: Some((10.0, 10.0)),
    })
    .expect("Scroll");

  let updated = wait_for_scroll_update(&ui_rx, tab_id);
  let frame = wait_for_frame_ready(&ui_rx, tab_id);

  assert!(
    updated.elements.len() >= 1,
    "expected at least one element scroll offset, got {:?}",
    updated.elements
  );
  assert!(
    updated.elements.values().any(|pt| pt.y > 0.0),
    "expected at least one element to scroll on y, got {:?}",
    updated.elements
  );
  assert_eq!(
    frame.scroll_state, updated,
    "FrameReady.scroll_state should match ScrollStateUpdated"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn scroll_clamps_to_zero() {
  let _lock = super::stage_listener_test_lock();
  let (_dir, url) = make_test_page();

  let handle = spawn_ui_worker("fastr-ui-worker-scroll-clamp-zero").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId(1);
  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
    })
    .expect("CreateTab");
  ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (200, 100),
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

  let _ = wait_for_frame_ready(&ui_rx, tab_id);

  // Ensure we're scrolled away from 0 so the clamp can be observed.
  ui_tx
    .send(UiToWorker::Scroll {
      tab_id,
      delta_css: (0.0, 120.0),
      pointer_css: None,
    })
    .expect("Scroll down");
  let _ = wait_for_scroll_update(&ui_rx, tab_id);
  let _ = wait_for_frame_ready(&ui_rx, tab_id);

  ui_tx
    .send(UiToWorker::Scroll {
      tab_id,
      delta_css: (0.0, -10_000.0),
      pointer_css: None,
    })
    .expect("Scroll up");
  let updated = wait_for_scroll_update(&ui_rx, tab_id);
  let frame = wait_for_frame_ready(&ui_rx, tab_id);

  assert!(
    updated.viewport.y.abs() < 1e-3,
    "expected viewport scroll to clamp to 0, got {:?}",
    updated.viewport
  );
  assert_eq!(
    frame.scroll_state, updated,
    "FrameReady.scroll_state should match ScrollStateUpdated"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
