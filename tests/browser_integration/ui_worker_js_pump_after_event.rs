#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{PointerButton, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

#[test]
fn ui_worker_pumps_js_after_click_event_and_repaints_on_mutation() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let index_url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body { margin: 0; padding: 0; }
            #box {
              position: absolute;
              left: 0;
              top: 0;
              width: 64px;
              height: 64px;
              background: rgb(255, 0, 0);
            }
            body[data-x="1"] #box {
              background: rgb(0, 255, 0);
            }
          </style>
        </head>
        <body>
          <div id="box"></div>
          <script>
            document.body.addEventListener("click", function () {
              setTimeout(function () {
                document.body.setAttribute("data-x", "1");
              }, 0);
            });
          </script>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-js-pump-after-event",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, Some(index_url.clone())))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (80, 80), 1.0))
    .expect("viewport");

  // Initial render: box should be red.
  let msg = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {index_url}"));
  let initial_frame = match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    WorkerToUi::NavigationFailed { url, error, .. } => panic!("navigation failed for {url}: {error}"),
    other => panic!("unexpected WorkerToUi while waiting for initial frame: {other:?}"),
  };
  assert_eq!(support::rgba_at(&initial_frame.pixmap, 10, 10), [255, 0, 0, 255]);

  // Drain follow-up messages from initial navigation so the next assertions are scoped to the click.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // Click the box. The click handler schedules a setTimeout(0) that mutates the DOM.
  //
  // The UI worker should pump the JS event loop after dispatching the click event so the timer task
  // runs immediately, and then schedule a repaint because JS mutated the DOM.
  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("pointer down");
  ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("pointer up");

  // Wait for the post-click repaint that reflects the JS mutation.
  let msg = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| match msg {
    WorkerToUi::FrameReady { frame, .. } => support::rgba_at(&frame.pixmap, 10, 10) == [0, 255, 0, 255],
    WorkerToUi::NavigationFailed { .. } => true,
    _ => false,
  })
  .unwrap_or_else(|| panic!("timed out waiting for JS-mutated green frame after click"));

  match msg {
    WorkerToUi::FrameReady { frame, .. } => {
      assert_eq!(support::rgba_at(&frame.pixmap, 10, 10), [0, 255, 0, 255]);
    }
    WorkerToUi::NavigationFailed { url, error, .. } => panic!("navigation failed for {url}: {error}"),
    other => panic!("unexpected WorkerToUi while waiting for green frame: {other:?}"),
  }

  drop(ui_tx);
  join.join().expect("worker join");
}

