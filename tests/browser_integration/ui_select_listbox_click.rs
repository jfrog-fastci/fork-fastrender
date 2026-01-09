#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  NavigationReason, PointerButton, RenderedFrame, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::worker::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

// Worker startup + first render can take a few seconds under parallel load (CI).
const TIMEOUT: Duration = Duration::from_secs(20);

fn recv_frame(rx: &Receiver<WorkerToUi>, tab_id: TabId, timeout: Duration) -> RenderedFrame {
  match support::recv_for_tab(rx, tab_id, timeout, |msg| matches!(msg, WorkerToUi::FrameReady { .. }))
  {
    Some(WorkerToUi::FrameReady { frame, .. }) => frame,
    Some(other) => panic!("expected FrameReady, got {other:?}"),
    None => panic!("timed out waiting for FrameReady"),
  }
}

#[test]
fn listbox_select_click_updates_selected_option_and_rerenders() {
  let site = support::TempSite::new();
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          select { display: block; width: 90px; height: 90px; padding: 0; border: 0; }
          #marker { width: 64px; height: 64px; background: rgb(255, 0, 0); }
          /* React to option[selected] mutation via :has so we can assert via pixels. */
          select:has(option#opt2[selected]) + #marker { background: rgb(0, 255, 0); }
        </style>
      </head>
      <body>
        <select size="3" id="sel">
          <option id="opt1">One</option>
          <option id="opt2">Two</option>
          <option id="opt3">Three</option>
        </select>
        <div id="marker"></div>
      </body>
    </html>
  "#;
  let url = site.write("page.html", html);

  let worker = spawn_ui_worker("fastr-ui-worker-select-listbox").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: Default::default(),
    })
    .expect("CreateTab");
  worker
    .ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (200, 200),
      dpr: 1.0,
    })
    .expect("ViewportChanged");
  worker
    .ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url,
      reason: NavigationReason::TypedUrl,
    })
    .expect("Navigate");

  let frame = recv_frame(&worker.ui_rx, tab_id, TIMEOUT);
  assert_eq!(
    support::rgba_at(&frame.pixmap, 10, 100),
    [255, 0, 0, 255],
    "expected marker to start red"
  );

  // Click the second row (row index 1) in the listbox.
  let click_pos = (10.0_f32, 45.0_f32);
  worker
    .ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: click_pos,
      button: PointerButton::Primary,
    })
    .expect("PointerDown");
  worker
    .ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: click_pos,
      button: PointerButton::Primary,
    })
    .expect("PointerUp");

  let deadline = Instant::now() + TIMEOUT;
  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    assert!(
      !remaining.is_zero(),
      "timed out waiting for marker to turn green after select click"
    );

    let msg = support::recv_for_tab(
      &worker.ui_rx,
      tab_id,
      remaining.min(Duration::from_millis(200)),
      |msg| matches!(msg, WorkerToUi::FrameReady { .. }),
    );
    let Some(WorkerToUi::FrameReady { frame, .. }) = msg else {
      continue;
    };

    if support::rgba_at(&frame.pixmap, 10, 100) == [0, 255, 0, 255] {
      break;
    }
  }

  worker.join().expect("join worker");
}
