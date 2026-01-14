#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::render_worker::{
  reset_scroll_hover_sync_count_for_test, scroll_hover_sync_count_for_test,
};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

// This test triggers real navigations + paints; keep timeout generous for contended CI hosts.
const TIMEOUT: Duration = Duration::from_secs(20);

fn next_navigation_committed(rx: &impl support::RecvTimeout<WorkerToUi>, tab_id: TabId) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationCommitted { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationCommitted for tab {tab_id:?}"));

  match msg {
    WorkerToUi::NavigationCommitted { .. } => {}
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}")
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn next_frame_ready(rx: &impl support::RecvTimeout<WorkerToUi>, tab_id: TabId) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  match msg {
    WorkerToUi::FrameReady { .. } => {}
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

#[test]
fn ui_worker_coalesces_scroll_induced_hover_syncs() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  reset_scroll_hover_sync_count_for_test();

  let site = support::TempSite::new();
  let url = site.write(
    "long.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      .row { height: 100px; }
    </style>
  </head>
  <body>
    <div class="row" style="background: rgb(255,0,0)"></div>
    <div class="row" style="background: rgb(0,255,0)"></div>
    <div class="row" style="background: rgb(0,0,255)"></div>
    <div class="row" style="background: rgb(255,255,0)"></div>
    <div class="row" style="background: rgb(0,255,255)"></div>
    <div class="row" style="background: rgb(255,0,255)"></div>
    <div class="row" style="background: rgb(128,128,128)"></div>
    <div class="row" style="background: rgb(64,64,64)"></div>
    <div class="row" style="background: rgb(192,192,192)"></div>
    <div class="row" style="background: rgb(0,0,0)"></div>
    <div class="row" style="background: rgb(255,255,255)"></div>
    <div class="row" style="background: rgb(10,10,10)"></div>
    <div class="row" style="background: rgb(20,20,20)"></div>
    <div class="row" style="background: rgb(30,30,30)"></div>
    <div class="row" style="background: rgb(40,40,40)"></div>
    <div class="row" style="background: rgb(50,50,50)"></div>
    <div class="row" style="background: rgb(60,60,60)"></div>
    <div class="row" style="background: rgb(70,70,70)"></div>
    <div class="row" style="background: rgb(80,80,80)"></div>
    <div class="row" style="background: rgb(90,90,90)"></div>
    <div class="row" style="background: rgb(100,100,100)"></div>
    <div class="row" style="background: rgb(110,110,110)"></div>
    <div class="row" style="background: rgb(120,120,120)"></div>
    <div class="row" style="background: rgb(130,130,130)"></div>
    <div class="row" style="background: rgb(140,140,140)"></div>
    <div class="row" style="background: rgb(150,150,150)"></div>
    <div class="row" style="background: rgb(160,160,160)"></div>
    <div class="row" style="background: rgb(170,170,170)"></div>
    <div class="row" style="background: rgb(180,180,180)"></div>
    <div class="row" style="background: rgb(190,190,190)"></div>
    <div class="row" style="background: rgb(200,200,200)"></div>
    <div class="row" style="background: rgb(210,210,210)"></div>
    <div class="row" style="background: rgb(220,220,220)"></div>
    <div class="row" style="background: rgb(230,230,230)"></div>
    <div class="row" style="background: rgb(240,240,240)"></div>
    <div class="row" style="background: rgb(250,250,250)"></div>
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-scroll-hover-coalesce").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("CreateTab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (128, 128), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("Navigate");

  next_navigation_committed(&ui_rx, tab_id);
  next_frame_ready(&ui_rx, tab_id);

  // Don't count any incidental hover syncs from startup; focus on the scroll burst below.
  reset_scroll_hover_sync_count_for_test();

  const SCROLL_BURST: usize = 100;
  for _ in 0..SCROLL_BURST {
    ui_tx
      .send(support::scroll_msg(tab_id, (0.0, 10.0), Some((10.0, 10.0))))
      .expect("Scroll");
  }

  // Wait for the coalesced scroll frame.
  next_frame_ready(&ui_rx, tab_id);

  let hover_syncs = scroll_hover_sync_count_for_test();
  assert!(
    hover_syncs <= 2,
    "expected scroll burst to coalesce hover hit-testing; sent {SCROLL_BURST} scroll messages, got {hover_syncs} hover syncs"
  );

  // Regression test: even if the UI only includes `pointer_css` on some scroll messages, the worker
  // should retain the last known pointer position for the burst and still run one coalesced hover
  // sync (instead of dropping to 0 due to the final scroll message having `pointer_css: None`).
  reset_scroll_hover_sync_count_for_test();
  for i in 0..SCROLL_BURST {
    ui_tx
      .send(support::scroll_msg(
        tab_id,
        (0.0, 10.0),
        if i == 0 { Some((10.0, 10.0)) } else { None },
      ))
      .expect("Scroll");
  }

  next_frame_ready(&ui_rx, tab_id);

  let hover_syncs = scroll_hover_sync_count_for_test();
  assert!(
    (1..=2).contains(&hover_syncs),
    "expected scroll burst with intermittent pointer_css to still run a coalesced hover sync; sent {SCROLL_BURST} scroll messages, got {hover_syncs} hover syncs"
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
