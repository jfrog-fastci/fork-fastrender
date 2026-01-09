#![cfg(feature = "browser_ui")]

use fastrender::interaction::scroll_wheel::{apply_wheel_scroll_at_point, ScrollWheelInput};
use fastrender::scroll::ScrollState;
use fastrender::ui::messages::{TabId, UiToWorker, WorkerToUi};
use fastrender::{FastRender, Point, RenderOptions, Size};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};
use tempfile::tempdir;

const TIMEOUT: Duration = Duration::from_secs(10);

fn wait_for_message<F>(rx: &Receiver<WorkerToUi>, timeout: Duration, mut f: F) -> WorkerToUi
where
  F: FnMut(&WorkerToUi) -> bool,
{
  let start = Instant::now();
  loop {
    let remaining = timeout
      .checked_sub(start.elapsed())
      .unwrap_or(Duration::from_secs(0));
    assert!(remaining > Duration::ZERO, "timed out waiting for worker message");
    let msg = rx
      .recv_timeout(remaining)
      .unwrap_or_else(|_| panic!("timed out waiting for worker message"));
    if f(&msg) {
      return msg;
    }
  }
}

fn drain_worker(rx: &Receiver<WorkerToUi>) {
  while rx.try_recv().is_ok() {}
}

fn spawn_worker() -> (Sender<UiToWorker>, Receiver<WorkerToUi>, std::thread::JoinHandle<()>) {
  let handle = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  (handle.tx, handle.rx, handle.join)
}

#[test]
fn scroll_snap_updates_viewport_scroll_state() {
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          html { scroll-snap-type: y mandatory; }
          .snap { height: 100px; scroll-snap-align: start; }
        </style>
      </head>
      <body>
        <div class="snap"></div>
        <div class="snap"></div>
      </body>
    </html>
  "#;
  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = format!("file://{}/index.html", dir.path().display());

  let (ui_tx, worker_rx, handle) = spawn_worker();
  let tab_id = TabId(1);
  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: Some(url),
      cancel: Default::default(),
    })
    .unwrap();
  ui_tx.send(UiToWorker::SetActiveTab { tab_id }).unwrap();
  ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (100, 100),
      dpr: 1.0,
    })
    .unwrap();

  let _ = wait_for_message(&worker_rx, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { tab_id: t, .. } if *t == tab_id)
  });
  drain_worker(&worker_rx);

  ui_tx
    .send(UiToWorker::Scroll {
      tab_id,
      delta_css: (0.0, 60.0),
      pointer_css: None,
    })
    .unwrap();

  let msg = wait_for_message(&worker_rx, TIMEOUT, |msg| match msg {
    WorkerToUi::ScrollStateUpdated { tab_id: t, scroll } if *t == tab_id => scroll.viewport.y > 0.0,
    _ => false,
  });
  let WorkerToUi::ScrollStateUpdated { scroll, .. } = msg else {
    unreachable!();
  };

  assert!(
    (scroll.viewport.y - 100.0).abs() < 1.0,
    "expected scroll snap to land at 100px, got {:?}",
    scroll.viewport
  );

  drop(ui_tx);
  handle.join().unwrap();
}

#[test]
fn element_scroll_at_pointer_updates_element_scroll_state() {
  let _lock = super::stage_listener_test_lock();
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #scroller {
            width: 100px;
            height: 50px;
            overflow: scroll;
            border: 1px solid black;
          }
          #content { height: 200px; }
        </style>
      </head>
      <body>
        <div id="scroller"><div id="content"></div></div>
      </body>
    </html>
  "#;

  let dir = tempdir().expect("temp dir");
  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = format!("file://{}/index.html", dir.path().display());

  // Discover the box id the renderer assigns to the scroller so we can assert on it. Use the same
  // `prepare_url` path as the browser worker so box ids match.
  let mut renderer = FastRender::new().expect("renderer");
  let report = renderer
    .prepare_url(
      &url,
      RenderOptions::new()
        .with_viewport(200, 200)
        .with_device_pixel_ratio(1.0),
    )
    .expect("prepare url");
  let scrolled = apply_wheel_scroll_at_point(
    report.document.fragment_tree(),
    &ScrollState::default(),
    Size::new(200.0, 200.0),
    Point::new(10.0, 10.0),
    ScrollWheelInput {
      delta_x: 0.0,
      delta_y: 20.0,
    },
  );
  let (&expected_box_id, _) = scrolled
    .elements
    .iter()
    .next()
    .expect("expected wheel scroll to hit #scroller");

  let (ui_tx, worker_rx, handle) = spawn_worker();
  let tab_id = TabId(1);
  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: Some(url),
      cancel: Default::default(),
    })
    .unwrap();
  ui_tx.send(UiToWorker::SetActiveTab { tab_id }).unwrap();
  ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (200, 200),
      dpr: 1.0,
    })
    .unwrap();

  let _ = wait_for_message(&worker_rx, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { tab_id: t, .. } if *t == tab_id)
  });
  drain_worker(&worker_rx);

  ui_tx
    .send(UiToWorker::Scroll {
      tab_id,
      delta_css: (0.0, 20.0),
      pointer_css: Some((10.0, 10.0)),
    })
    .unwrap();

  let msg = wait_for_message(&worker_rx, TIMEOUT, |msg| match msg {
    WorkerToUi::ScrollStateUpdated { tab_id: t, scroll } if *t == tab_id => {
      scroll.elements.contains_key(&expected_box_id)
    }
    _ => false,
  });
  let WorkerToUi::ScrollStateUpdated { scroll, .. } = msg else {
    unreachable!();
  };

  let offset = scroll
    .elements
    .get(&expected_box_id)
    .copied()
    .expect("expected element scroll offset for scroller box id");
  assert!(
    offset.y > 0.0,
    "expected element scroll y offset > 0, got {offset:?}"
  );

  drop(ui_tx);
  handle.join().unwrap();
}
