#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  NavigationReason, PointerButton, RenderedFrame, RepaintReason, TabId, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::sync::mpsc::Receiver;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn next_frame_ready(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  match msg {
    WorkerToUi::FrameReady {
      tab_id: got_tab,
      frame,
    } => {
      assert_eq!(got_tab, tab_id);
      frame
    }
    WorkerToUi::NavigationFailed {
      tab_id: got_tab,
      url,
      error,
      ..
    } => {
      assert_eq!(got_tab, tab_id);
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn expect_pixel_rgba(frame: &RenderedFrame, x: u32, y: u32, expected: [u8; 4]) {
  let got = support::rgba_at(&frame.pixmap, x, y);
  assert_eq!(got, expected, "unexpected pixel at ({x},{y})");
}

#[test]
fn ui_worker_slot_mouseenter_fires_when_hovering_slotted_light_dom_content() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #host { position: absolute; left: 0; top: 0; width: 64px; height: 64px; }
            #light { width: 64px; height: 64px; background: rgb(0, 0, 255); }
            #indicator { position: absolute; left: 100px; top: 0; width: 64px; height: 64px; background: rgb(255, 0, 0); }
          </style>
        </head>
        <body>
          <div id="host">
            <template shadowrootmode="open">
              <style>
                slot { display: block; width: 100%; height: 100%; }
              </style>
              <slot id="s"></slot>
            </template>
            <div id="light"></div>
          </div>
          <div id="indicator"></div>
          <script>
            window.__log = [];
            const slot = document.getElementById('host').shadowRoot.getElementById('s');
            slot.addEventListener('mouseenter', () => {
              window.__log.push('slotmouseenter');
              document.getElementById('indicator').style.background = 'rgb(0, 255, 0)';
            });
          </script>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-slot-mouseenter",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("viewport");
  ui_tx
    .send(support::navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("navigate");

  // The indicator starts red.
  let frame = next_frame_ready(&ui_rx, tab_id);
  expect_pixel_rgba(&frame, 110, 10, [255, 0, 0, 255]);
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // Start from outside the viewport, then move onto the slotted light-DOM content.
  ui_tx
    .send(support::pointer_move(
      tab_id,
      (-1.0, -1.0),
      PointerButton::None,
    ))
    .expect("pointer move out");
  ui_tx
    .send(support::pointer_move(tab_id, (10.0, 10.0), PointerButton::None))
    .expect("pointer move in");
  // Force a paint even if hover does not affect rendering (the JS handler mutates styles).
  ui_tx
    .send(support::request_repaint(tab_id, RepaintReason::Explicit))
    .expect("repaint");

  let frame = next_frame_ready(&ui_rx, tab_id);
  expect_pixel_rgba(&frame, 110, 10, [0, 255, 0, 255]);

  drop(ui_tx);
  join.join().expect("worker join");
}

