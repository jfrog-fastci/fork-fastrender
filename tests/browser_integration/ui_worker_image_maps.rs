#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{CursorKind, NavigationReason, PointerButton, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn next_frame_ready(rx: &impl support::RecvTimeout<WorkerToUi>, tab_id: TabId) {
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
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
) -> (Option<String>, CursorKind) {
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
    other => panic!("expected HoverChanged, got {other:?}"),
  }
}

#[test]
fn client_side_image_map_hover_and_click_navigates_area_href() {
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
      #img {{ display: block; width: 100px; height: 100px; }}
    </style>
  </head>
  <body>
    <a href="outer.html">
      <img id="img" usemap="#m" src="{image_src}">
    </a>
    <map name="m">
      <area id="a1" href="dest.html" shape="rect" coords="0,0,50,50">
    </map>
  </body>
</html>
"##
    ),
  );
  let _outer_url = site.write("outer.html", "<!doctype html><html><body>outer</body></html>");
  let _dest_url = site.write("dest.html", "<!doctype html><html><body>dest</body></html>");

  let expected_dest = url::Url::parse(&page_url)
    .expect("parse page url")
    .join("dest.html")
    .expect("resolve dest.html")
    .to_string();

  let handle = spawn_ui_worker("fastr-ui-worker-image-map-usemap").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 200), 1.0))
    .expect("viewport");
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      page_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate");

  next_frame_ready(&ui_rx, tab_id);

  // Drain any navigation/paint bookkeeping so the hover+click assertions only see fresh output.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  // Hover inside the mapped rectangle (top-left 50x50).
  ui_tx
    .send(support::pointer_move(
      tab_id,
      (10.0, 10.0),
      PointerButton::None,
    ))
    .expect("pointer move");
  let (hovered_url, cursor) = next_hover_changed(&ui_rx, tab_id);
  assert_eq!(cursor, CursorKind::Pointer);
  assert_eq!(hovered_url.as_deref(), Some(expected_dest.as_str()));

  // Primary click should navigate to the <area> href, not the outer <a> href.
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

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationCommitted { url, .. } if url == &expected_dest
    )
  })
  .unwrap_or_else(|| {
    let msgs = support::drain_for(&ui_rx, Duration::from_millis(200));
    panic!(
      "timed out waiting for NavigationCommitted({expected_dest}); saw:\n{}",
      support::format_messages(&msgs)
    );
  });

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn server_side_image_map_click_appends_coords_query() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let image_src = "data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='100'%20height='100'%3E%3C/svg%3E";

  let page_url = site.write(
    "index.html",
    &format!(
      r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body {{ margin: 0; padding: 0; }}
      #img {{ display: block; width: 100px; height: 100px; }}
    </style>
  </head>
  <body>
    <a href="dest.html"><img id="img" ismap src="{image_src}"></a>
  </body>
</html>
"#
    ),
  );
  let _dest_url = site.write("dest.html", "<!doctype html><html><body>dest</body></html>");

  // Click inside the image at non-integer coordinates to verify the engine floors.
  let click_pos = (10.9, 20.1);
  let expected_query = "10,20";
  let expected_dest = url::Url::parse(&page_url)
    .expect("parse page url")
    .join(&format!("dest.html?{expected_query}"))
    .expect("resolve dest.html?x,y")
    .to_string();

  let handle = spawn_ui_worker("fastr-ui-worker-image-map-ismap").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 200), 1.0))
    .expect("viewport");
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      page_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate");

  next_frame_ready(&ui_rx, tab_id);
  let _ = support::drain_for(&ui_rx, Duration::from_millis(200));

  ui_tx
    .send(support::pointer_down(
      tab_id,
      click_pos,
      PointerButton::Primary,
    ))
    .expect("pointer down");
  ui_tx
    .send(support::pointer_up(
      tab_id,
      click_pos,
      PointerButton::Primary,
    ))
    .expect("pointer up");

  let committed = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::NavigationCommitted { .. })
  })
  .unwrap_or_else(|| {
    let msgs = support::drain_for(&ui_rx, Duration::from_millis(200));
    panic!(
      "timed out waiting for NavigationCommitted after server-side image map click; saw:\n{}",
      support::format_messages(&msgs)
    );
  });

  let committed_url = match committed {
    WorkerToUi::NavigationCommitted { url, .. } => url,
    other => panic!("expected NavigationCommitted, got {other:?}"),
  };

  assert_eq!(
    committed_url, expected_dest,
    "expected <img ismap> click to navigate to dest.html?x,y; got {committed_url}"
  );
  let parsed = url::Url::parse(&committed_url).expect("parse committed URL");
  assert_eq!(
    parsed.query(),
    Some(expected_query),
    "expected ismap query string to match floored local coords; url={committed_url}"
  );

  drop(ui_tx);
  join.join().expect("worker join");
}
