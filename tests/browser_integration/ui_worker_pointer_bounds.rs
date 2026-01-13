#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{CursorKind, NavigationReason, PointerButton, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::Duration;
use url::Url;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn next_frame_ready(rx: &Receiver<WorkerToUi>, tab_id: TabId) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  if let WorkerToUi::NavigationFailed { url, error, .. } = msg {
    panic!("navigation failed for {url}: {error}");
  }
}

fn next_hover_changed(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> (Option<String>, CursorKind) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::HoverChanged { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for HoverChanged for tab {tab_id:?}"));
  match msg {
    WorkerToUi::HoverChanged {
      hovered_url,
      cursor,
      ..
    } => (hovered_url, cursor),
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn assert_no_navigation_or_frame(msgs: &[WorkerToUi], tab_id: TabId) {
  for msg in msgs {
    match msg {
      WorkerToUi::FrameReady { tab_id: got, .. }
      | WorkerToUi::NavigationStarted { tab_id: got, .. }
      | WorkerToUi::NavigationCommitted { tab_id: got, .. }
      | WorkerToUi::NavigationFailed { tab_id: got, .. } if *got == tab_id => {
        panic!(
          "unexpected interaction side effect for out-of-bounds pointer event:\n{}",
          support::format_messages(msgs)
        );
      }
      _ => {}
    }
  }
}

#[test]
fn pointer_move_outside_viewport_is_treated_as_leave_for_hover_hit_testing() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page_url = site.write(
    "index.html",
    r##"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body { margin: 0; padding: 0; }
            a { position: absolute; top: 10px; display: block; width: 80px; height: 24px; background: rgb(220, 220, 0); }
            #in { left: 10px; }
            /* This link is outside the viewport width, but still within the page coordinate space. */
            #off { left: 150px; }
          </style>
        </head>
        <body>
          <a id="in" href="in.html">In</a>
          <a id="off" href="off.html">Off</a>
        </body>
      </html>
    "##,
  );
  site.write("in.html", "<!doctype html><html><body>in</body></html>");
  site.write("off.html", "<!doctype html><html><body>off</body></html>");

  let expected_in = Url::parse(&page_url)
    .expect("parse base url")
    .join("in.html")
    .expect("resolve in.html")
    .to_string();

  let worker = spawn_ui_worker("fastr-ui-worker-pointer-bounds-hover").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (120, 80), 1.0))
    .unwrap();
  worker
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      page_url,
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  next_frame_ready(&worker.ui_rx, tab_id);

  // Hover the on-screen link.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 15.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Pointer);
  assert_eq!(hovered_url.as_deref(), Some(expected_in.as_str()));

  // Move the pointer to a coordinate that would hit-test the off-screen link *if* treated as in-page
  // input. It should instead behave like the leave sentinel (clear hover).
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (150.0, 15.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Default);
  assert_eq!(hovered_url, None);

  worker.join().unwrap();
}

#[test]
fn pointer_click_outside_viewport_does_not_navigate_or_trigger_interaction_repaint() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let page_url = site.write(
    "index.html",
    r##"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body { margin: 0; padding: 0; }
            a { position: absolute; top: 10px; display: block; width: 80px; height: 24px; background: rgb(220, 220, 0); }
            /* Place the link outside the viewport, but inside the page coordinate space. */
            #off { left: 150px; }
          </style>
        </head>
        <body>
          <a id="off" href="dest.html">Offscreen</a>
        </body>
      </html>
    "##,
  );
  site.write("dest.html", "<!doctype html><html><body>dest</body></html>");

  let worker = spawn_ui_worker("fastr-ui-worker-pointer-bounds-click").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (120, 80), 1.0))
    .unwrap();
  worker
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      page_url,
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  next_frame_ready(&worker.ui_rx, tab_id);
  let _ = support::drain_for(&worker.ui_rx, Duration::from_millis(50));

  // Click the off-screen link coordinate. This should be treated like an out-of-page click:
  // - no :active repaint on PointerDown
  // - no navigation on PointerUp
  worker
    .ui_tx
    .send(support::pointer_down(
      tab_id,
      (150.0, 15.0),
      PointerButton::Primary,
    ))
    .unwrap();
  let msgs = support::drain_for(&worker.ui_rx, Duration::from_millis(500));
  assert_no_navigation_or_frame(&msgs, tab_id);

  worker
    .ui_tx
    .send(support::pointer_up(
      tab_id,
      (150.0, 15.0),
      PointerButton::Primary,
    ))
    .unwrap();
  let msgs = support::drain_for(&worker.ui_rx, Duration::from_millis(500));
  assert_no_navigation_or_frame(&msgs, tab_id);

  worker.join().unwrap();
}

