#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::interaction::hit_test::{
  hit_test_dom_call_count_for_test, reset_hit_test_dom_call_count_for_test,
  set_hit_test_dom_counting_enabled_for_test,
};
use fastrender::ui::messages::{NavigationReason, PointerButton, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;

// UI worker startup + rendering can take a few seconds when tests run in parallel.
const TIMEOUT: Duration = Duration::from_secs(20);

fn wait_for_frame_ready(rx: &impl support::RecvTimeout<WorkerToUi>, tab_id: TabId) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));
  match msg {
    WorkerToUi::FrameReady { .. } => {}
    other => panic!("unexpected message while waiting for FrameReady: {other:?}"),
  }
}

#[test]
fn ui_worker_pointer_down_up_reuses_interaction_engine_hit_test() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "page.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      #t { width: 64px; height: 64px; background: rgb(255, 0, 0); }
    </style>
  </head>
  <body>
    <div id="t"></div>
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-pointer-hit-test-dedup").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("CreateTab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(support::navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");
  wait_for_frame_ready(&ui_rx, tab_id);

  // Count hit tests only for the pointer events below.
  set_hit_test_dom_counting_enabled_for_test(true);
  reset_hit_test_dom_call_count_for_test();

  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("PointerDown");
  wait_for_frame_ready(&ui_rx, tab_id);
  assert_eq!(
    hit_test_dom_call_count_for_test(),
    1,
    "expected exactly one hit_test_dom call for PointerDown when layout artifacts are available",
  );

  ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("PointerUp");
  wait_for_frame_ready(&ui_rx, tab_id);
  assert_eq!(
    hit_test_dom_call_count_for_test(),
    2,
    "expected exactly one additional hit_test_dom call for PointerUp when layout artifacts are available",
  );

  set_hit_test_dom_counting_enabled_for_test(false);
  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

#[test]
fn ui_worker_pointer_move_reuses_interaction_engine_hit_test() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "page.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      #t { width: 64px; height: 64px; background: rgb(255, 0, 0); }
    </style>
  </head>
  <body>
    <div id="t"></div>
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-pointer-move-hit-test-dedup").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("CreateTab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(support::navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");
  wait_for_frame_ready(&ui_rx, tab_id);

  // Count hit tests only for the pointer move below.
  set_hit_test_dom_counting_enabled_for_test(true);
  reset_hit_test_dom_call_count_for_test();

  ui_tx
    .send(support::pointer_move(tab_id, (10.0, 10.0), PointerButton::None))
    .expect("PointerMove");
  wait_for_frame_ready(&ui_rx, tab_id);
  assert_eq!(
    hit_test_dom_call_count_for_test(),
    1,
    "expected exactly one hit_test_dom call for PointerMove when layout artifacts are available",
  );

  set_hit_test_dom_counting_enabled_for_test(false);
  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
