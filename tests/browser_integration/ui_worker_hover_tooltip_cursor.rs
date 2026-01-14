#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  CursorKind, NavigationReason, PointerButton, RepaintReason, TabId, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn next_frame_ready(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
) -> fastrender::ui::messages::RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn next_hover_changed(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
) -> (Option<String>, CursorKind, Option<String>) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::HoverChanged { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for HoverChanged for tab {tab_id:?}"));

  match msg {
    WorkerToUi::HoverChanged {
      hovered_url,
      cursor,
      tooltip,
      ..
    } => (hovered_url, cursor, tooltip),
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

#[test]
fn hover_changed_reports_tooltip_from_title_attributes() {
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
            #parent { position: absolute; top: 10px; left: 10px; width: 120px; height: 24px; background: rgb(220, 220, 0); }
            #child { display: block; width: 100%; height: 100%; }
            #no_title { position: absolute; top: 50px; left: 10px; width: 120px; height: 24px; background: rgb(0, 0, 220); }
          </style>
        </head>
        <body>
          <div id="parent" title="ParentTitle">
            <span id="child" title="   "></span>
          </div>
          <div id="no_title"></div>
        </body>
      </html>
    "##,
  );

  let worker = spawn_ui_worker("fastr-ui-worker-hover-tooltip").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (240, 120), 1.0))
    .unwrap();
  worker
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      page_url,
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  let _ = next_frame_ready(&worker.ui_rx, tab_id);

  // Hover the child span (whitespace-only title): should fall back to parent title.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 15.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor, tooltip) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(hovered_url, None);
  assert_eq!(cursor, CursorKind::Default);
  assert_eq!(tooltip.as_deref(), Some("ParentTitle"));

  // Hover an element without title: tooltip clears.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 55.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor, tooltip) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(hovered_url, None);
  assert_eq!(cursor, CursorKind::Default);
  assert_eq!(tooltip, None);

  // Hover the titled element again.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 15.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor, tooltip) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(hovered_url, None);
  assert_eq!(cursor, CursorKind::Default);
  assert_eq!(tooltip.as_deref(), Some("ParentTitle"));

  // Leaving the page clears tooltip state.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (-1.0, -1.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor, tooltip) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(hovered_url, None);
  assert_eq!(cursor, CursorKind::Default);
  assert_eq!(tooltip, None);

  worker.join().unwrap();
}

#[test]
fn hover_changed_reports_cursor_from_computed_css_cursor() {
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
            summary { position: absolute; top: 10px; left: 10px; width: 120px; height: 24px; background: rgb(220, 220, 0); }
            #crosshair { position: absolute; top: 40px; left: 10px; width: 120px; height: 24px; background: rgb(10, 10, 10); cursor: crosshair; }
            #grab { position: absolute; top: 70px; left: 10px; width: 120px; height: 24px; background: rgb(10, 10, 10); cursor: grab; }
            #disabled { position: absolute; top: 100px; left: 10px; width: 120px; height: 24px; }
          </style>
        </head>
        <body>
          <details open>
            <summary id="summary">Summary</summary>
          </details>
          <div id="crosshair"></div>
          <div id="grab"></div>
          <input id="disabled" type="text" disabled value="">
        </body>
      </html>
    "##,
  );

  let worker = spawn_ui_worker("fastr-ui-worker-hover-css-cursor").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (240, 140), 1.0))
    .unwrap();
  worker
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      page_url,
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  let _ = next_frame_ready(&worker.ui_rx, tab_id);

  // <summary> has `cursor: pointer` in the UA stylesheet.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 15.0),
      PointerButton::None,
    ))
    .unwrap();
  let (_hovered_url, cursor, _tooltip) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Pointer);

  // Author CSS: cursor: crosshair.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 45.0),
      PointerButton::None,
    ))
    .unwrap();
  let (_hovered_url, cursor, _tooltip) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Crosshair);

  // Author CSS: cursor: grab.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 75.0),
      PointerButton::None,
    ))
    .unwrap();
  let (_hovered_url, cursor, _tooltip) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Grab);

  // Disabled text input should not show the I-beam when UA CSS sets `cursor: default`.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 110.0),
      PointerButton::None,
    ))
    .unwrap();
  let (_hovered_url, cursor, _tooltip) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Default);

  worker.join().unwrap();
}

#[test]
fn pointer_leave_sentinel_clears_hover_even_for_negative_positioned_content() {
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
            #neg {
              position: absolute;
              left: -10px;
              top: -10px;
              width: 20px;
              height: 20px;
              background: rgb(255, 0, 0);
              cursor: crosshair;
            }
            #neg:hover {
              background: rgb(0, 255, 0);
            }
          </style>
        </head>
        <body>
          <div id="neg" title="Neg"></div>
        </body>
      </html>
    "##,
  );

  let worker = spawn_ui_worker("fastr-ui-worker-hover-negative-leave").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (40, 40), 1.0))
    .unwrap();
  worker
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      page_url,
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  assert_eq!(support::rgba_at(&frame.pixmap, 0, 0), [255, 0, 0, 255]);

  // Hover the element (in the visible portion at the top-left).
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (1.0, 1.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor, tooltip) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(hovered_url, None);
  assert_eq!(cursor, CursorKind::Crosshair);
  assert_eq!(tooltip.as_deref(), Some("Neg"));

  worker
    .ui_tx
    .send(support::request_repaint(tab_id, RepaintReason::Explicit))
    .unwrap();
  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame.pixmap, 0, 0),
    [0, 255, 0, 255],
    "expected :hover to apply while cursor is over the element"
  );

  // Leave the page image (UI sentinel position). This must clear hover state, even though the
  // sentinel page-point (-1,-1) would otherwise intersect the negative-positioned element.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (-1.0, -1.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor, tooltip) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(hovered_url, None);
  assert_eq!(cursor, CursorKind::Default);
  assert_eq!(tooltip, None);

  worker
    .ui_tx
    .send(support::request_repaint(tab_id, RepaintReason::Explicit))
    .unwrap();
  let frame = next_frame_ready(&worker.ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame.pixmap, 0, 0),
    [255, 0, 0, 255],
    "expected :hover to clear after pointer leaves the page"
  );

  worker.join().unwrap();
}
