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
fn scroll_state_updates_sync_js_tab_element_from_point_in_click_handler() {
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
            body { height: 2000px; }
            a { position: absolute; left: 0; width: 180px; height: 40px; display: block; }
            #allow { top: 0; background: rgb(200, 200, 200); }
            #block { top: 1000px; background: rgb(100, 100, 255); }
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
    "fastr-ui-worker-js-scroll-sync",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, Some(index_url.clone())))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("viewport initial");

  wait_for_frame(&ui_rx, tab_id, (200, 120));
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // Scroll so that `#block` is at the top of the viewport. The click handler uses
  // `document.elementFromPoint(ev.clientX, ev.clientY)` and calls `preventDefault()` only when
  // `#block` is under the pointer.
  //
  // Regression: the UI worker updated scroll state for the renderer document but not for
  // `tab.js_tab`, so elementFromPoint observed the *old* scroll offset and did not prevent
  // navigation.
  ui_tx
    .send(support::scroll_to_msg(tab_id, (0.0, 1000.0)))
    .expect("scroll to");
  let (_frame, scroll) = support::wait_for_frame_and_scroll_state_updated(&ui_rx, tab_id, TIMEOUT);
  assert!(
    (scroll.viewport.y - 1000.0).abs() < 0.5,
    "expected scroll y≈1000 after ScrollTo, got {}",
    scroll.viewport.y
  );
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // Sanity-check that the hover hit-testing (renderer DOM) sees the scrolled link.
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
        "expected hover URL to match the blocked link after scroll"
      );
      assert_eq!(cursor, fastrender::ui::messages::CursorKind::Pointer);
    }
    other => panic!("unexpected message: {other:?}"),
  }
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // Click. The click handler should call preventDefault() and suppress the navigation.
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
    "expected click preventDefault (driven by elementFromPoint) to suppress navigation after ScrollTo; got:\n{}",
    support::format_messages(&msgs)
  );

  drop(ui_tx);
  join.join().expect("worker join");
}
