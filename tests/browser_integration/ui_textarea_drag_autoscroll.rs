#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(20);

fn next_frame_ready(rx: &impl support::RecvTimeout<WorkerToUi>, tab_id: TabId) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));
  assert!(
    matches!(msg, WorkerToUi::FrameReady { .. }),
    "unexpected message while waiting for FrameReady: {msg:?}"
  );
}

#[test]
fn ui_textarea_selection_drag_autoscrolls() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body { margin: 0; padding: 0; }
            #ta {
              position: absolute;
              left: 0;
              top: 0;
              width: 300px;
              height: 60px;
              padding: 0;
              border: 0;
              outline: none;
              font-family: "Noto Sans Mono", monospace;
              font-size: 20px;
            }
          </style>
        </head>
        <body>
          <textarea id="ta">line 1
line 2
line 3
line 4
line 5
line 6
line 7
line 8
line 9
line 10
line 11
line 12
line 13
line 14
line 15
line 16
line 17
line 18
line 19
line 20
line 21
line 22
line 23
line 24
line 25
line 26
line 27
line 28
line 29
line 30</textarea>
        </body>
      </html>
    "#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-textarea-drag-autoscroll").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: Some(url),
      cancel: Default::default(),
    })
    .expect("create tab");
  ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (360, 200),
      dpr: 1.0,
    })
    .expect("viewport");
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id })
    .expect("active tab");

  // Initial paint (ensures layout cache exists for hit testing + textarea geometry).
  next_frame_ready(&ui_rx, tab_id);

  // Start a selection drag inside the textarea.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 20.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .expect("pointer down");

  // Drag past the bottom edge of the textarea (still within the viewport).
  ui_tx
    .send(UiToWorker::PointerMove {
      tab_id,
      pos_css: (10.0, 140.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer move");

  let msg = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| match msg {
    WorkerToUi::ScrollStateUpdated { scroll, .. } => {
      scroll.elements.values().any(|offset| offset.y > 0.0)
    }
    _ => false,
  })
  .unwrap_or_else(|| panic!("timed out waiting for textarea autoscroll ScrollStateUpdated"));

  match msg {
    WorkerToUi::ScrollStateUpdated { scroll, .. } => {
      assert!(
        scroll.elements.values().any(|offset| offset.y > 0.0),
        "expected textarea element scroll_y to increase during drag (got scroll_state={scroll:?})"
      );
    }
    other => panic!("unexpected message while waiting for ScrollStateUpdated: {other:?}"),
  }

  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 140.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .expect("pointer up");

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
