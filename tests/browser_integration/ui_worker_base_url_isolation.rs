#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  NavigationReason, PointerButton, RenderedFrame, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::worker::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::Duration;

// These tests spin up real UI worker threads that create renderers and rasterize frames, so allow a
// little extra time on contended CI hosts.
const TIMEOUT: Duration = Duration::from_secs(20);

fn next_navigation_committed(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> String {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationCommitted { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationCommitted for tab {tab_id:?}"));

  match msg {
    WorkerToUi::NavigationCommitted { url, .. } => url,
    WorkerToUi::NavigationFailed { url, error, .. } => panic!("navigation failed for {url}: {error}"),
    other => panic!("unexpected WorkerToUi message while waiting for NavigationCommitted: {other:?}"),
  }
}

fn next_frame_ready(rx: &Receiver<WorkerToUi>, tab_id: TabId) -> RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| matches!(msg, WorkerToUi::FrameReady { .. }))
    .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("unexpected WorkerToUi message while waiting for FrameReady: {other:?}"),
  }
}

fn write_site(
  site: &support::TempSite,
  label: &str,
  index_bg_rgb: (u8, u8, u8),
  next_bg_rgb: (u8, u8, u8),
) -> (String, String) {
  site.write(
    "style.css",
    &format!(
      r#"
html, body {{ margin: 0; padding: 0; }}
html.index, body.index {{ background: rgb({},{},{}); }}
html.next, body.next {{ background: rgb({},{},{}); }}
"#,
      index_bg_rgb.0,
      index_bg_rgb.1,
      index_bg_rgb.2,
      next_bg_rgb.0,
      next_bg_rgb.1,
      next_bg_rgb.2,
    ),
  );

  let index_url = site.write(
    "index.html",
    &format!(
      r#"<!doctype html>
<html class="index">
  <head>
    <meta charset="utf-8">
    <title>Site {label} Index</title>
    <link rel="stylesheet" href="style.css">
    <style>
      /* Keep the link a stable hit target at the top-left for deterministic pointer input. */
      #link {{
        position: absolute;
        top: 0;
        left: 0;
        width: 100px;
        height: 40px;
        display: block;
        background: rgb(255, 0, 0);
      }}
    </style>
  </head>
  <body class="index">
    <a id="link" href="next.html"></a>
  </body>
</html>
"#,
    ),
  );

  let next_url = site.write(
    "next.html",
    &format!(
      r#"<!doctype html>
<html class="next">
  <head>
    <meta charset="utf-8">
    <title>Site {label} Next</title>
    <link rel="stylesheet" href="style.css">
  </head>
  <body class="next"></body>
</html>
"#,
    ),
  );

  (index_url, next_url)
}

#[test]
fn relative_url_and_subresource_resolution_is_isolated_per_tab() {
  let _lock = super::stage_listener_test_lock();
  let site_a = support::TempSite::new();
  let site_b = support::TempSite::new();

  let (a_index, a_next) = write_site(&site_a, "A", (0, 0, 255), (0, 255, 0));
  let (b_index, b_next) = write_site(&site_b, "B", (255, 255, 0), (255, 0, 255));

  let handle = spawn_ui_worker("fastr-ui-worker-base-url-isolation").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab1 = TabId::new();
  let tab2 = TabId::new();

  for tab_id in [tab1, tab2] {
    ui_tx
      .send(UiToWorker::CreateTab {
        tab_id,
        initial_url: None,
        cancel: Default::default(),
      })
      .expect("create tab");
    ui_tx
      .send(UiToWorker::ViewportChanged {
        tab_id,
        viewport_css: (240, 160),
        dpr: 1.0,
      })
      .expect("viewport");
  }

  // Navigate tab1 first, then tab2, so a buggy "global base_url" would end up holding site B before
  // we click in tab1.
  ui_tx
    .send(UiToWorker::Navigate {
      tab_id: tab1,
      url: a_index.clone(),
      reason: NavigationReason::TypedUrl,
    })
    .expect("navigate tab1");
  assert_eq!(next_navigation_committed(&ui_rx, tab1), a_index);
  let frame_a_index = next_frame_ready(&ui_rx, tab1);
  assert_eq!(support::rgba_at(&frame_a_index.pixmap, 150, 80), [0, 0, 255, 255]);

  ui_tx
    .send(UiToWorker::Navigate {
      tab_id: tab2,
      url: b_index.clone(),
      reason: NavigationReason::TypedUrl,
    })
    .expect("navigate tab2");
  assert_eq!(next_navigation_committed(&ui_rx, tab2), b_index);
  let frame_b_index = next_frame_ready(&ui_rx, tab2);
  assert_eq!(
    support::rgba_at(&frame_b_index.pixmap, 150, 80),
    [255, 255, 0, 255]
  );

  // Click link in tab1 and confirm it resolves to site A /next.html (not site B).
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id: tab1,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .expect("pointer down tab1");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id: tab1,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .expect("pointer up tab1");
  assert_eq!(next_navigation_committed(&ui_rx, tab1), a_next);
  let frame_a_next = next_frame_ready(&ui_rx, tab1);
  assert_eq!(support::rgba_at(&frame_a_next.pixmap, 150, 80), [0, 255, 0, 255]);

  // Now tab1 is the most recent navigation. Clicking in tab2 must still resolve using site B's base.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id: tab2,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .expect("pointer down tab2");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id: tab2,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .expect("pointer up tab2");
  assert_eq!(next_navigation_committed(&ui_rx, tab2), b_next);
  let frame_b_next = next_frame_ready(&ui_rx, tab2);
  assert_eq!(
    support::rgba_at(&frame_b_next.pixmap, 150, 80),
    [255, 0, 255, 255]
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
