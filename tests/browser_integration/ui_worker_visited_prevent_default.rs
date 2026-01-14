#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{PointerButton, RenderedFrame, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::sync::mpsc::Receiver;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

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
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| matches!(msg, WorkerToUi::FrameReady { .. }))
    .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));
  match msg {
    WorkerToUi::FrameReady { tab_id: got, frame } => {
      assert_eq!(got, tab_id);
      frame
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

#[test]
fn ui_worker_prevent_default_does_not_mark_link_visited() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let _next_url = site.write(
    "next.html",
    r#"<!doctype html>
      <html><body>next</body></html>"#,
  );
  let index_url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; background: rgb(255, 255, 255); }
            a {
              position: absolute;
              left: 0;
              width: 180px;
              height: 50px;
              display: block;
              background: rgb(255, 0, 0);
            }
            a:visited { background: rgb(0, 0, 255); }
            #normal { top: 0; }
            #prevent { top: 60px; }
          </style>
        </head>
        <body>
          <!-- Use target=_blank so the default action doesn't navigate away from the current page. -->
          <a id="normal" href="next.html" target="_blank">normal</a>
          <a id="prevent" href="next.html">prevent</a>
          <script>
            var prevent = document.getElementById("prevent");
            prevent.addEventListener("click", function (ev) { ev.preventDefault(); });
            prevent.addEventListener("auxclick", function (ev) { ev.preventDefault(); });
          </script>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-visited-prevent-default",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, Some(index_url.clone())))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (220, 140), 1.0))
    .expect("viewport");

  let frame = next_frame_ready(&ui_rx, tab_id);
  expect_rgb_at_css(&frame, 10, 10, (255, 0, 0));
  expect_rgb_at_css(&frame, 10, 70, (255, 0, 0));
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // ---------------------------------------------------------------------------
  // 1) Baseline: a normal (non-prevented) click should mark the link as visited.
  // ---------------------------------------------------------------------------
  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("pointer down normal");
  let _ = next_frame_ready(&ui_rx, tab_id);
  ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("pointer up normal");
  let frame = next_frame_ready(&ui_rx, tab_id);
  expect_rgb_at_css(&frame, 10, 10, (0, 0, 255));
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // ---------------------------------------------------------------------------
  // 2) Primary click with click.preventDefault(): should NOT mark visited.
  // ---------------------------------------------------------------------------
  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 70.0),
      PointerButton::Primary,
    ))
    .expect("pointer down prevent");
  let _ = next_frame_ready(&ui_rx, tab_id);
  ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 70.0),
      PointerButton::Primary,
    ))
    .expect("pointer up prevent");
  let frame = next_frame_ready(&ui_rx, tab_id);
  expect_rgb_at_css(&frame, 10, 70, (255, 0, 0));
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // ---------------------------------------------------------------------------
  // 3) Middle click with auxclick.preventDefault(): should NOT mark visited.
  // ---------------------------------------------------------------------------
  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 70.0),
      PointerButton::Middle,
    ))
    .expect("pointer down auxclick prevent");
  let _ = next_frame_ready(&ui_rx, tab_id);
  ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 70.0),
      PointerButton::Middle,
    ))
    .expect("pointer up auxclick prevent");
  let frame = next_frame_ready(&ui_rx, tab_id);
  expect_rgb_at_css(&frame, 10, 70, (255, 0, 0));

  drop(ui_tx);
  join.join().expect("worker join");
}

