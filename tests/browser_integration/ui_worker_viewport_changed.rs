#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{NavigationReason, RenderedFrame, TabId, WorkerToUi};
use fastrender::ui::worker_loop::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

// Worker startup + the first navigation can take a few seconds under load when integration tests
// run in parallel on CI; keep this timeout generous to avoid flakiness.
const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn scroll_fixture() -> (support::TempSite, String) {
  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <title>Scroll Fixture</title>
    <style>
      html, body { margin: 0; padding: 0; }
      .spacer { height: 4000px; background: linear-gradient(#eee, #ccc); }
    </style>
  </head>
  <body>
    <div class="spacer">scroll</div>
  </body>
</html>
"#,
  );
  (site, url)
}

fn wait_for_navigation_committed(rx: &Receiver<WorkerToUi>, tab_id: TabId, expected_url: &str) {
  let start = Instant::now();
  let mut seen: Vec<WorkerToUi> = Vec::new();
  loop {
    let remaining = TIMEOUT.saturating_sub(start.elapsed());
    if remaining.is_zero() {
      panic!(
        "timed out waiting for NavigationCommitted for tab {tab_id:?} (expected {expected_url}). Messages:\n{}",
        support::format_messages(&seen)
      );
    }
    let slice = remaining.min(Duration::from_millis(25));
    match rx.recv_timeout(slice) {
      Ok(msg) => {
        match msg {
          WorkerToUi::NavigationCommitted {
            tab_id: got, url, ..
          } if got == tab_id => {
            assert_eq!(
              url, expected_url,
              "NavigationCommitted URL mismatch for tab {tab_id:?}"
            );
            return;
          }
          WorkerToUi::NavigationFailed {
            tab_id: got,
            url,
            error,
            ..
          } if got == tab_id => {
            panic!("navigation failed for {url}: {error}");
          }
          other => {
            // Cap the debug buffer to keep worst-case memory use bounded if the worker spams
            // messages.
            if seen.len() < 64 {
              seen.push(other);
            }
          }
        }
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
        panic!(
          "worker disconnected while waiting for NavigationCommitted for tab {tab_id:?}. Messages:\n{}",
          support::format_messages(&seen)
        );
      }
    }
  }
}

fn wait_for_frame_with_meta(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  expected_viewport_css: (u32, u32),
  expected_dpr: f32,
) -> RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| match msg {
    WorkerToUi::FrameReady { frame, .. } => {
      frame.viewport_css == expected_viewport_css && (frame.dpr - expected_dpr).abs() < 1e-6
    }
    WorkerToUi::NavigationFailed { .. } => true,
    _ => false,
  })
  .unwrap_or_else(|| {
    panic!(
      "timed out waiting for FrameReady for tab {tab_id:?} (viewport_css={expected_viewport_css:?}, dpr={expected_dpr})"
    )
  });

  match msg {
    WorkerToUi::FrameReady { tab_id: got, frame } => {
      assert_eq!(got, tab_id);
      frame
    }
    WorkerToUi::NavigationFailed { tab_id: got, url, error, .. } => {
      assert_eq!(got, tab_id);
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn assert_pixmap_matches_viewport(frame: &RenderedFrame) {
  let expected_w = ((frame.viewport_css.0 as f32) * frame.dpr).round().max(1.0) as i64;
  let expected_h = ((frame.viewport_css.1 as f32) * frame.dpr).round().max(1.0) as i64;
  let actual_w = frame.pixmap.width() as i64;
  let actual_h = frame.pixmap.height() as i64;

  // Allow a small tolerance for rounding differences between the layout/paint pipeline and the
  // test calculation.
  assert!(
    (actual_w - expected_w).abs() <= 1,
    "pixmap width mismatch: expected≈{expected_w}, got {actual_w} (viewport_css={:?}, dpr={})",
    frame.viewport_css,
    frame.dpr
  );
  assert!(
    (actual_h - expected_h).abs() <= 1,
    "pixmap height mismatch: expected≈{expected_h}, got {actual_h} (viewport_css={:?}, dpr={})",
    frame.viewport_css,
    frame.dpr
  );
}

#[test]
fn viewport_changed_does_not_repaint_before_first_navigation() {
  let _lock = super::stage_listener_test_lock();
  let handle = spawn_ui_worker("fastr-ui-worker-viewport-changed-no-nav").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("CreateTab");

  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 100), 2.0))
    .expect("ViewportChanged");

  // ViewportChanged should not emit frames until we have navigated at least once. Emitting a blank
  // frame is wasteful and creates UI flakiness for clients that wait on the first navigation frame.
  let msgs = support::drain_for(&ui_rx, Duration::from_millis(300));
  let saw_frame = msgs.iter().any(|msg| {
    matches!(
      msg,
      WorkerToUi::FrameReady { tab_id: got, .. } if *got == tab_id
    )
  });
  assert!(
    !saw_frame,
    "unexpected FrameReady before navigation for tab {tab_id:?}:\n{}",
    support::format_messages(&msgs)
  );

  drop(ui_tx);
  join.join().unwrap();
}

#[test]
fn viewport_changed_after_navigation_emits_new_frame_with_updated_dimensions() {
  let _lock = super::stage_listener_test_lock();
  let site = support::TempSite::new();
  let url = site.write(
    "page.html",
    r#"<!doctype html>
<html>
  <head>
    <style>html, body { margin: 0; padding: 0; background: rgb(10, 20, 30); }</style>
  </head>
  <body></body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-viewport-changed-a").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  // Keep the initial navigation small so the test is fast.
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .unwrap();
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url.clone(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  wait_for_navigation_committed(&ui_rx, tab_id, &url);
  let _initial = wait_for_frame_with_meta(&ui_rx, tab_id, (64, 64), 1.0);

  ui_tx
    .send(support::viewport_changed_msg(tab_id, (120, 80), 1.0))
    .unwrap();

  let frame = wait_for_frame_with_meta(&ui_rx, tab_id, (120, 80), 1.0);
  assert_eq!(frame.viewport_css, (120, 80));
  assert!((frame.dpr - 1.0).abs() < 1e-6);
  assert_pixmap_matches_viewport(&frame);

  drop(ui_tx);
  join.join().unwrap();
}

#[test]
fn viewport_changed_updates_dpr_and_pixmap_scale() {
  let _lock = super::stage_listener_test_lock();
  let site = support::TempSite::new();
  let url = site.write(
    "page.html",
    r#"<!doctype html>
<html>
  <head>
    <style>html, body { margin: 0; padding: 0; background: rgb(80, 90, 100); }</style>
  </head>
  <body></body>
</html>
"#,
  );

  let handle = spawn_ui_worker("fastr-ui-worker-viewport-changed-b").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .unwrap();
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (90, 60), 1.0))
    .unwrap();
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url.clone(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  wait_for_navigation_committed(&ui_rx, tab_id, &url);
  let frame_1x = wait_for_frame_with_meta(&ui_rx, tab_id, (90, 60), 1.0);
  let w1 = frame_1x.pixmap.width() as i64;
  let h1 = frame_1x.pixmap.height() as i64;
  assert_pixmap_matches_viewport(&frame_1x);

  ui_tx
    .send(support::viewport_changed_msg(tab_id, (90, 60), 2.0))
    .unwrap();

  let frame_2x = wait_for_frame_with_meta(&ui_rx, tab_id, (90, 60), 2.0);
  assert!((frame_2x.dpr - 2.0).abs() < 1e-6);
  assert_pixmap_matches_viewport(&frame_2x);

  let w2 = frame_2x.pixmap.width() as i64;
  let h2 = frame_2x.pixmap.height() as i64;
  assert!(
    (w2 - w1 * 2).abs() <= 1,
    "expected pixmap width to scale ~2x when dpr doubles: {w1} -> {w2}"
  );
  assert!(
    (h2 - h1 * 2).abs() <= 1,
    "expected pixmap height to scale ~2x when dpr doubles: {h1} -> {h2}"
  );

  drop(ui_tx);
  join.join().unwrap();
}

#[test]
fn viewport_changed_repaints_after_navigation_and_preserves_scroll() {
  let _lock = super::stage_listener_test_lock();
  let (_site, url) = scroll_fixture();

  let handle = spawn_ui_worker("fastr-ui-worker-viewport-changed-scroll").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("CreateTab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (160, 120), 1.0))
    .expect("ViewportChanged initial");
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("Navigate");

  wait_for_navigation_committed(&ui_rx, tab_id, &url);
  let _ = wait_for_frame_with_meta(&ui_rx, tab_id, (160, 120), 1.0);

  ui_tx
    .send(support::scroll_msg(tab_id, (0.0, 250.0), None))
    .expect("Scroll");
  let scrolled = wait_for_frame_with_meta(&ui_rx, tab_id, (160, 120), 1.0);
  assert!(
    scrolled.scroll_state.viewport.y > 0.0,
    "expected non-zero scroll after scrolling, got {:?}",
    scrolled.scroll_state.viewport
  );

  ui_tx
    .send(support::viewport_changed_msg(tab_id, (220, 180), 2.0))
    .expect("ViewportChanged resize");
  let resized = wait_for_frame_with_meta(&ui_rx, tab_id, (220, 180), 2.0);
  assert_pixmap_matches_viewport(&resized);
  assert!(
    resized.scroll_state.viewport.y > 0.0,
    "expected viewport scroll to be preserved across ViewportChanged, got {:?}",
    resized.scroll_state.viewport
  );

  drop(ui_tx);
  join.join().unwrap();
}

#[test]
fn viewport_changed_coalesces_to_latest_state() {
  let _lock = super::stage_listener_test_lock();
  let site = support::TempSite::new();
  let url = site.write(
    "page.html",
    r#"<!doctype html>
<html>
  <head>
    <style>html, body { margin: 0; padding: 0; background: rgb(40, 50, 60); }</style>
  </head>
  <body></body>
</html>
"#,
  );

  let handle =
    spawn_ui_worker("fastr-ui-worker-viewport-changed-coalesce").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("CreateTab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (160, 120), 1.0))
    .expect("ViewportChanged initial");
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("Navigate");

  wait_for_navigation_committed(&ui_rx, tab_id, &url);
  let _ = wait_for_frame_with_meta(&ui_rx, tab_id, (160, 120), 1.0);

  ui_tx
    .send(support::viewport_changed_msg(tab_id, (100, 80), 1.0))
    .expect("ViewportChanged 1");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (123, 97), 2.0))
    .expect("ViewportChanged 2");

  let frame = wait_for_frame_with_meta(&ui_rx, tab_id, (123, 97), 2.0);
  assert_pixmap_matches_viewport(&frame);

  drop(ui_tx);
  join.join().unwrap();
}
