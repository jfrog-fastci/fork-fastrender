#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{PointerButton, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn wait_for_frame(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
  viewport: (u32, u32),
) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| match msg {
    WorkerToUi::FrameReady { frame, .. } => frame.viewport_css == viewport,
    WorkerToUi::NavigationFailed { .. } => true,
    _ => false,
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  match msg {
    WorkerToUi::FrameReady { tab_id: got, frame } => {
      assert_eq!(got, tab_id);
      assert_eq!(frame.viewport_css, viewport);
    }
    WorkerToUi::NavigationFailed {
      tab_id: got,
      url,
      error,
      ..
    } => {
      assert_eq!(got, tab_id);
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

#[test]
fn viewport_changed_syncs_js_tab_viewport_for_element_from_point_in_click_handler() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let blocked_url = site.write(
    "blocked.html",
    r#"<!doctype html>
      <html><head><style>html, body { margin: 0; padding: 0; background: rgb(255, 0, 0); }</style></head>
      <body>blocked</body></html>"#,
  );
  let _allowed_url = site.write(
    "allowed.html",
    r#"<!doctype html>
      <html><head><style>html, body { margin: 0; padding: 0; background: rgb(0, 255, 0); }</style></head>
      <body>allowed</body></html>"#,
  );
  let index_url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body { margin: 0; padding: 0; }
            a { position: absolute; left: 0; top: 0; width: 180px; height: 40px; display: block; }
            #allow { background: rgb(200, 200, 200); }
            /* Wide viewport: allow link at (0,0), block link away from the click point. */
            #block { left: 220px; background: rgb(100, 100, 255); }
            /* Narrow viewport: swap so block link is at (0,0), allow moves down. */
            @media (max-width: 300px) {
              #block { left: 0; top: 0; }
              #allow { left: 0; top: 60px; }
            }
          </style>
        </head>
        <body>
          <a id="allow" href="allowed.html">allow</a>
          <a id="block" href="blocked.html">block</a>
          <script>
            document.addEventListener("click", function (ev) {
              const el = document.elementFromPoint(ev.clientX, ev.clientY);
              if (el && el.id === "block") {
                ev.preventDefault();
              }
            });
          </script>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-js-viewport-sync",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, Some(index_url.clone())))
    .expect("create tab");
  // Start wide so the JS tab is created with a larger viewport. We'll later resize narrower and
  // verify JS geometry APIs observe the updated size.
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (400, 120), 1.0))
    .expect("viewport initial");

  wait_for_frame(&ui_rx, tab_id, (400, 120));
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("viewport resize");
  wait_for_frame(&ui_rx, tab_id, (200, 120));
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // Sanity-check that the click point is over the "blocked" link in the *rendered* (legacy) tab
  // after resizing.
  ui_tx
    .send(support::pointer_move(
      tab_id,
      (10.0, 10.0),
      PointerButton::None,
    ))
    .expect("pointer move");
  let hover = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::HoverChanged { .. })
  })
  .expect("HoverChanged");
  match hover {
    WorkerToUi::HoverChanged {
      tab_id: got,
      hovered_url,
      cursor,
      ..
    } => {
      assert_eq!(got, tab_id);
      assert_eq!(
        hovered_url.as_deref(),
        Some(blocked_url.as_str()),
        "expected hover URL to match the blocked link after resize"
      );
      assert_eq!(cursor, fastrender::ui::messages::CursorKind::Pointer);
    }
    other => panic!("unexpected message: {other:?}"),
  }
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // Click. The click handler consults `document.elementFromPoint` and calls `preventDefault()` only
  // when the element under the pointer is `#block`. This depends on the viewport size (media query).
  //
  // Regression: the UI worker updated viewport size/DPR on the legacy `BrowserDocument` but not on
  // `tab.js_tab`, so `elementFromPoint` observed the *old* viewport and did not prevent navigation.
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

  let msgs = support::drain_for(&ui_rx, Duration::from_millis(500));
  assert!(
    !msgs.iter().any(|msg| {
      matches!(
        msg,
        WorkerToUi::NavigationStarted { .. }
          | WorkerToUi::NavigationCommitted { .. }
          | WorkerToUi::NavigationFailed { .. }
          | WorkerToUi::RequestOpenInNewTab { .. }
          | WorkerToUi::RequestOpenInNewTabRequest { .. }
      )
    }),
    "expected click preventDefault (driven by elementFromPoint) to suppress navigation after ViewportChanged; got:\n{}",
    support::format_messages(&msgs)
  );

  drop(ui_tx);
  join.join().expect("worker join");
}
