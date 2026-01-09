#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  NavigationReason, PointerButton, RenderedFrame, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::worker::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);

fn fixture() -> (support::TempSite, String) {
  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #box { width:64px; height:64px; background: rgb(255,0,0); }
      #box[data-fastr-hover="true"] { background: rgb(0,255,0); }
      #box[data-fastr-active="true"] { background: rgb(0,0,255); }
    </style>
  </head>
  <body>
    <div id="box"></div>
  </body>
</html>
"#,
  );
  (site, url)
}

fn rgba_at_css(frame: &RenderedFrame, x_css: u32, y_css: u32) -> [u8; 4] {
  let x_px = ((x_css as f32) * frame.dpr).round() as u32;
  let y_px = ((y_css as f32) * frame.dpr).round() as u32;
  support::rgba_at(&frame.pixmap, x_px, y_px)
}

fn expect_rgb_at_css(frame: &RenderedFrame, x_css: u32, y_css: u32, expected: (u8, u8, u8)) {
  let rgba = rgba_at_css(frame, x_css, y_css);
  assert_eq!(
    (rgba[0], rgba[1], rgba[2], rgba[3]),
    (expected.0, expected.1, expected.2, 255),
    "unexpected pixel at ({x_css},{y_css}) css px"
  );
}

fn next_frame_ready(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  match msg {
    WorkerToUi::FrameReady {
      tab_id: msg_tab,
      frame,
    } => {
      assert_eq!(msg_tab, tab_id);
      frame
    }
    WorkerToUi::NavigationFailed {
      tab_id: msg_tab,
      url,
      error,
    } => {
      assert_eq!(msg_tab, tab_id);
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

#[test]
fn pointer_move_sets_hover_and_repaints() {
  let (_site, url) = fixture();

  let worker = spawn_ui_worker("fastr-ui-worker-hover-active-a").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
    })
    .unwrap();
  worker
    .ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (256, 256),
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

  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  expect_rgb_at_css(&frame, 10, 10, (255, 0, 0));

  worker
    .ui_tx
    .send(UiToWorker::PointerMove {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::None,
    })
    .unwrap();
  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  expect_rgb_at_css(&frame, 10, 10, (0, 255, 0));

  worker
    .ui_tx
    .send(UiToWorker::PointerMove {
      tab_id,
      pos_css: (200.0, 200.0),
      button: PointerButton::None,
    })
    .unwrap();
  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  expect_rgb_at_css(&frame, 10, 10, (255, 0, 0));

  worker.join().unwrap();
}

#[test]
fn pointer_down_sets_active_until_pointer_up() {
  let (_site, url) = fixture();

  let worker = spawn_ui_worker("fastr-ui-worker-hover-active-b").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
    })
    .unwrap();
  worker
    .ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (256, 256),
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

  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  expect_rgb_at_css(&frame, 10, 10, (255, 0, 0));

  // Make hover deterministic so PointerUp resolves to green rather than red.
  worker
    .ui_tx
    .send(UiToWorker::PointerMove {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::None,
    })
    .unwrap();
  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  expect_rgb_at_css(&frame, 10, 10, (0, 255, 0));

  worker
    .ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .unwrap();
  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  expect_rgb_at_css(&frame, 10, 10, (0, 0, 255));

  worker
    .ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .unwrap();
  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  expect_rgb_at_css(&frame, 10, 10, (0, 255, 0));

  worker.join().unwrap();
}

