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
fn hover_changed_reports_link_url_and_cursor_kind() {
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
             #link { position: absolute; top: 10px; left: 10px; display: block; width: 120px; height: 24px; background: rgb(220, 220, 0); }
             /* Link placed far outside the viewport so out-of-bounds pointer coords would still hit it
                if the worker incorrectly considers them "in page". */
             #far { position: absolute; top: 9990px; left: 9990px; display: block; width: 120px; height: 24px; background: rgb(0, 220, 220); }
              #input { position: absolute; top: 50px; left: 10px; width: 140px; height: 24px; border: 1px solid #000; }
              #empty { position: absolute; top: 90px; left: 10px; width: 140px; height: 24px; background: rgb(10, 10, 10); }
              #button { position: absolute; top: 120px; left: 10px; width: 140px; height: 24px; }
              #select { position: absolute; top: 150px; left: 10px; width: 140px; height: 24px; }
            </style>
          </head>
          <body>
            <a id="link" href="dest.html#frag">Link</a>
            <a id="far" href="far.html">Far</a>
            <input id="input" type="text" value="">
            <div id="empty"></div>
            <button id="button" type="button">Button</button>
            <select id="select"><option>One</option><option>Two</option></select>
          </body>
       </html>
     "##,
  );

  let expected_hover_url = url::Url::parse(&page_url)
    .expect("parse base url")
    .join("dest.html#frag")
    .expect("resolve href")
    .to_string();

  let worker = spawn_ui_worker("fastr-ui-worker-hover-cursor").expect("spawn ui worker");
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

  // Hover the link.
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
  assert_eq!(hovered_url.as_deref(), Some(expected_hover_url.as_str()));

  // Hovering the same position again should not emit a duplicate HoverChanged message. By sending the
  // next PointerMove to a different target before waiting, we ensure we'd observe any unwanted
  // duplicate HoverChanged first.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 15.0),
      PointerButton::None,
    ))
    .unwrap();

  // Hover the input.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 60.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Text);
  assert_eq!(hovered_url, None);

  // Move to non-interactive region: hovered_url should clear.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 100.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Default);
  assert_eq!(hovered_url, None);

  // Hover the link again.
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
  assert_eq!(hovered_url.as_deref(), Some(expected_hover_url.as_str()));

  // Out-of-bounds pointer coordinates (outside viewport) should clear hover state. Some UI
  // front-ends send these values instead of the (-1,-1) sentinel.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (9999.0, 9999.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Default);
  assert_eq!(hovered_url, None);

  // Hover the link again.
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
  assert_eq!(hovered_url.as_deref(), Some(expected_hover_url.as_str()));

  // Leaving the page via sentinel coordinates should also clear hover state.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (-1.0, -1.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Default);
  assert_eq!(hovered_url, None);

  // Hover input again to force a cursor transition, then ensure buttons don't use the I-beam.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 60.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Text);
  assert_eq!(hovered_url, None);

  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 130.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Pointer);
  assert_eq!(hovered_url, None);

  // Hover input again, then ensure selects don't use the I-beam.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 60.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Text);
  assert_eq!(hovered_url, None);

  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 155.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Default);
  assert_eq!(hovered_url, None);

  worker.join().unwrap();
}

#[test]
fn hover_changed_respects_computed_css_cursor_keywords() {
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
            .box { position: absolute; left: 10px; width: 140px; height: 24px; background: rgb(220, 220, 0); }
            #crosshair { top: 10px; cursor: crosshair; }
            #grab { top: 40px; cursor: grab; }
            #grabbing { top: 70px; cursor: grabbing; }
            #disabled { position: absolute; top: 100px; left: 10px; width: 140px; height: 24px; border: 1px solid #000; }
            /* Explicitly override UA `cursor: default` back to `auto` to ensure disabled controls still
               avoid the I-beam cursor under our fallback heuristics. */
            #disabled_auto { position: absolute; top: 130px; left: 10px; width: 140px; height: 24px; border: 1px solid #000; cursor: auto; }
            #details { position: absolute; top: 160px; left: 10px; margin: 0; }
            #summary { display: block; width: 140px; height: 24px; background: rgb(180, 180, 255); }
          </style>
        </head>
        <body>
          <div id="crosshair" class="box">crosshair</div>
          <div id="grab" class="box">grab</div>
          <div id="grabbing" class="box">grabbing</div>
          <input id="disabled" type="text" disabled value="disabled">
          <input id="disabled_auto" type="text" disabled value="disabled cursor auto">
          <details id="details" open>
            <summary id="summary">Summary</summary>
            <div>Details content</div>
          </details>
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
    .send(support::viewport_changed_msg(tab_id, (256, 220), 1.0))
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

  // Custom CSS cursor: crosshair.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 15.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Crosshair);
  assert_eq!(hovered_url, None);

  // Custom CSS cursor: grab.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 45.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Grab);
  assert_eq!(hovered_url, None);

  // Custom CSS cursor: grabbing.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 75.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Grabbing);
  assert_eq!(hovered_url, None);

  // UA stylesheet cursor: disabled text inputs should use `cursor: default`.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 110.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Default);
  assert_eq!(hovered_url, None);

  // UA stylesheet cursor: `<summary>` should use `cursor: pointer`.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 165.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Pointer);
  assert_eq!(hovered_url, None);

  // Explicit `cursor: auto` override should still avoid the I-beam for disabled text inputs.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 140.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Default);
  assert_eq!(hovered_url, None);

  worker.join().unwrap();
}

#[test]
fn hover_updates_after_viewport_scroll_without_pointer_position() {
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
            a { display: block; width: 160px; height: 30px; background: rgb(220, 220, 0); }
            .spacer { height: 600px; }
            .top { height: 10px; }
            .bottom { height: 2000px; }
          </style>
        </head>
        <body>
          <div class="top"></div>
          <a id="link1" href="a.html">Link 1</a>
          <div class="spacer"></div>
          <a id="link2" href="b.html">Link 2</a>
          <div class="bottom"></div>
        </body>
      </html>
    "##,
  );

  let expected_link1 = url::Url::parse(&page_url)
    .expect("parse base url")
    .join("a.html")
    .expect("resolve link1 href")
    .to_string();
  let expected_link2 = url::Url::parse(&page_url)
    .expect("parse base url")
    .join("b.html")
    .expect("resolve link2 href")
    .to_string();

  let worker = spawn_ui_worker("fastr-ui-worker-hover-scroll").expect("spawn ui worker");
  let tab_id = TabId(1);
  worker
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  worker
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (256, 200), 1.0))
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

  // Hover link 1 (near the top of the viewport).
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 20.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Pointer);
  assert_eq!(hovered_url.as_deref(), Some(expected_link1.as_str()));

  worker
    .ui_tx
    .send(support::scroll_msg(tab_id, (0.0, 640.0), None))
    .unwrap();
  let (hovered_url, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Pointer);
  assert_eq!(hovered_url.as_deref(), Some(expected_link2.as_str()));

  worker.join().unwrap();
}
