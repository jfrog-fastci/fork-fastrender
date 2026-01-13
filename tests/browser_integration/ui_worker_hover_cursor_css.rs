#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{CursorKind, NavigationReason, PointerButton, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::Duration;

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

#[test]
fn hover_changed_cursor_respects_css_cursor_overrides_on_links() {
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
            a { position: absolute; left: 10px; display: block; width: 160px; height: 24px; background: rgb(220, 220, 0); }
            #default { top: 10px; cursor: default; }
            #crosshair { top: 40px; cursor: crosshair; }
            #grab { top: 70px; cursor: grab; }
            #grabbing { top: 100px; cursor: grabbing; }
          </style>
        </head>
        <body>
          <a id="default" href="dest.html#frag">Default cursor</a>
          <a id="crosshair" href="cross.html">Crosshair cursor</a>
          <a id="grab" href="grab.html">Grab cursor</a>
          <a id="grabbing" href="grabbing.html">Grabbing cursor</a>
        </body>
      </html>
    "##,
  );

  let base = url::Url::parse(&page_url).expect("parse base url");
  let expected_default = base
    .join("dest.html#frag")
    .expect("resolve href")
    .to_string();
  let expected_crosshair = base.join("cross.html").expect("resolve href").to_string();
  let expected_grab = base.join("grab.html").expect("resolve href").to_string();
  let expected_grabbing = base
    .join("grabbing.html")
    .expect("resolve href")
    .to_string();

  let worker = spawn_ui_worker("fastr-ui-worker-hover-cursor-css").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (256, 160), 1.0))
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

  // `cursor: default` must override UA link cursor (usually pointer), but hovered_url should still
  // resolve for the link.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 15.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Default);
  assert_eq!(hovered_url.as_deref(), Some(expected_default.as_str()));

  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 45.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Crosshair);
  assert_eq!(hovered_url.as_deref(), Some(expected_crosshair.as_str()));

  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 75.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Grab);
  assert_eq!(hovered_url.as_deref(), Some(expected_grab.as_str()));

  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 105.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Grabbing);
  assert_eq!(hovered_url.as_deref(), Some(expected_grabbing.as_str()));

  worker.join().unwrap();
}

#[test]
fn hover_changed_reports_expanded_css_cursor_kinds() {
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
            #abbr { position: absolute; top: 10px; left: 10px; display: block; width: 140px; height: 24px; background: rgb(220, 220, 0); }
            #zoom { position: absolute; top: 50px; left: 10px; width: 140px; height: 24px; background: rgb(0, 220, 220); cursor: zoom-in; }
            #ew { position: absolute; top: 90px; left: 10px; width: 140px; height: 24px; background: rgb(220, 0, 220); cursor: ew-resize; }
          </style>
        </head>
        <body>
          <abbr id="abbr" title="Abbreviation title">Abbr</abbr>
          <div id="zoom"></div>
          <div id="ew"></div>
        </body>
      </html>
    "##,
  );

  let worker = spawn_ui_worker("fastr-ui-worker-hover-cursor-css-expanded").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (256, 140), 1.0))
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

  // UA stylesheet cursor: `abbr[title] { cursor: help; }`.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 15.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Help);
  assert_eq!(hovered_url, None);

  // Author CSS cursor: zoom-in.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 60.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::ZoomIn);
  assert_eq!(hovered_url, None);

  // Author CSS cursor: ew-resize.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 100.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::EwResize);
  assert_eq!(hovered_url, None);

  worker.join().unwrap();
}
