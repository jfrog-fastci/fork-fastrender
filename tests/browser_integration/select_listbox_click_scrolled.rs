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
  let start = Instant::now();
  let mut seen = Vec::new();
  loop {
    let remaining = timeout.saturating_sub(start.elapsed());
    assert!(
      !remaining.is_zero(),
      "timed out waiting for FrameReady\n{}",
      support::format_messages(&seen)
    );
    let msg = support::recv_for_tab(rx, tab_id, remaining.min(Duration::from_millis(200)), |_| true);
    let Some(msg) = msg else {
      continue;
    };
    match msg {
      WorkerToUi::FrameReady { frame, .. } => return frame,
      WorkerToUi::NavigationFailed { url, error, .. } => {
        panic!(
          "navigation failed while waiting for FrameReady ({url}): {error}\n{}",
          support::format_messages(&seen)
        );
      }
      other => seen.push(other),
    }
  }
}

#[test]
fn select_listbox_click_accounts_for_scroll_offset() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; background: rgb(0,0,0); }
          #lb {
            display: block;
            background: rgb(0,0,0);
            color: rgba(0,0,0,0);
            accent-color: rgb(255,0,0);
            line-height: 20px;
            font-size: 16px;
            border: 0;
            padding: 0;
            width: 160px;
            height: 60px; /* 3 rows */
            overflow-y: auto;
            overflow-x: hidden;
          }
          #marker { width: 64px; height: 64px; background: rgb(255,0,0); }
          /* React to option[selected] mutation via :has so we can assert via pixels. */
          #lb:has(option#opt11[selected]) + #marker { background: rgb(0,255,0); }
        </style>
      </head>
      <body>
        <select id="lb" size="3">
          <option id="opt1">Option 1</option>
          <option id="opt2">Option 2</option>
          <option id="opt3">Option 3</option>
          <option id="opt4">Option 4</option>
          <option id="opt5">Option 5</option>
          <option id="opt6">Option 6</option>
          <option id="opt7">Option 7</option>
          <option id="opt8">Option 8</option>
          <option id="opt9">Option 9</option>
          <option id="opt10">Option 10</option>
          <option id="opt11">Option 11</option>
          <option id="opt12">Option 12</option>
          <option id="opt13">Option 13</option>
          <option id="opt14">Option 14</option>
          <option id="opt15">Option 15</option>
          <option id="opt16">Option 16</option>
          <option id="opt17">Option 17</option>
          <option id="opt18">Option 18</option>
          <option id="opt19">Option 19</option>
          <option id="opt20">Option 20</option>
        </select>
        <div id="marker"></div>
      </body>
    </html>
  "#;
  let url = site.write("index.html", html);

  let worker =
    spawn_ui_worker("fastr-ui-worker-select-listbox-click-scrolled").expect("spawn ui worker");

  let tab_id = TabId::new();
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
    support::rgba_at(&frame.pixmap, 10, 80),
    [255, 0, 0, 255],
    "expected marker to start red"
  );

  // Scroll the listbox so the default selected option (row 0) is offscreen.
  worker
    .ui_tx
    .send(UiToWorker::Scroll {
      tab_id,
      delta_css: (0.0, 200.0),
      pointer_css: Some((10.0, 10.0)),
    })
    .expect("Scroll");
  let frame_after_scroll = recv_frame(&worker.ui_rx, tab_id, TIMEOUT);
  assert_eq!(
    support::rgba_at(&frame_after_scroll.pixmap, 10, 80),
    [255, 0, 0, 255],
    "expected marker to remain red after scrolling"
  );

  // Click near the top of the listbox. When scroll offsets are respected, this activates the first
  // visible row (not the original row 0).
  let click_pos = (10.0_f32, 10.0_f32);
  worker
    .ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: click_pos,
      button: PointerButton::Primary,
    })
    .expect("PointerDown 2");
  worker
    .ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: click_pos,
      button: PointerButton::Primary,
    })
    .expect("PointerUp 2");

  let deadline = Instant::now() + TIMEOUT;
  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    assert!(
      !remaining.is_zero(),
      "timed out waiting for marker to turn green after scrolled listbox click"
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

    if support::rgba_at(&frame.pixmap, 10, 80) == [0, 255, 0, 255] {
      break;
    }
  }

  worker.join().expect("join worker");
}
