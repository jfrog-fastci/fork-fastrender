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

fn next_hover_changed(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
) -> (Option<String>, Option<String>, CursorKind) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::HoverChanged { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for HoverChanged for tab {tab_id:?}"));
  match msg {
    WorkerToUi::HoverChanged {
      hovered_url,
      tooltip,
      cursor,
      ..
    } => (hovered_url, tooltip, cursor),
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

#[test]
fn hover_changed_reports_tooltip_for_image_map_area_title() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();

  // Keep the intrinsic image size aligned with the CSS size so the area coordinate system is
  // unambiguous and matches browsers.
  let image_src = "data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='100'%20height='100'%3E%3C/svg%3E";

  let page_url = site.write(
    "index.html",
    &format!(
      r##"<!doctype html>
        <html>
          <head>
            <meta charset="utf-8">
            <style>
              html, body {{ margin: 0; padding: 0; }}
              #box {{ position: absolute; left: 10px; top: 10px; width: 120px; height: 24px; background: rgb(220, 220, 0); }}
              #img {{ position: absolute; left: 10px; top: 50px; width: 100px; height: 100px; }}
            </style>
          </head>
          <body>
            <div id="box" title="Div tip"></div>
            <map id="m" name="m">
              <area href="dest.html" title="Area tip" shape="rect" coords="0,0,50,50">
            </map>
            <img id="img" usemap="#m" src="{image_src}">
          </body>
        </html>
      "##,
    ),
  );
  let _dest_url = site.write("dest.html", "<!doctype html><html><body>dest</body></html>");

  let expected_hover_url = url::Url::parse(&page_url)
    .expect("parse base url")
    .join("dest.html")
    .expect("resolve href")
    .to_string();

  let worker = spawn_ui_worker("fastr-ui-worker-hover-tooltip-image-map").expect("spawn ui worker");
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
  // Drain any navigation/paint bookkeeping so hover assertions only see fresh output.
  let _ = support::drain_for(&worker.ui_rx, Duration::from_millis(200));

  // Hover the div with a `title`.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (15.0, 15.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, tooltip, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Default);
  assert_eq!(hovered_url, None);
  assert_eq!(tooltip.as_deref(), Some("Div tip"));

  // Hover inside the image-map area region.
  worker
    .ui_tx
    .send(support::pointer_move(
      tab_id,
      (20.0, 60.0),
      PointerButton::None,
    ))
    .unwrap();
  let (hovered_url, tooltip, cursor) = next_hover_changed(&worker.ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Pointer);
  assert_eq!(hovered_url.as_deref(), Some(expected_hover_url.as_str()));
  assert_eq!(tooltip.as_deref(), Some("Area tip"));

  worker.join().unwrap();
}
