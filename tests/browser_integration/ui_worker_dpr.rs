#![cfg(feature = "browser_ui")]

use super::support::{create_tab_msg, navigate_msg, viewport_changed_msg};
use fastrender::ui::messages::{NavigationReason, RenderedFrame, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::worker_loop::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

// Under parallel test load, render worker threads can take a while to produce the first frame.
const FRAME_TIMEOUT: Duration = Duration::from_secs(20);

fn recv_frame_ready(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  timeout: Duration,
) -> RenderedFrame {
  let deadline = Instant::now() + timeout;
  loop {
    let now = Instant::now();
    if now >= deadline {
      panic!("timed out waiting for FrameReady for {tab_id:?}");
    }
    let remaining = deadline - now;
    match rx.recv_timeout(remaining) {
      Ok(WorkerToUi::FrameReady {
        tab_id: msg_tab,
        frame,
      }) if msg_tab == tab_id => return frame,
      Ok(_) => {}
      Err(err) => panic!("recv_timeout while waiting for FrameReady: {err:?}"),
    }
  }
}

fn assert_close(actual: f32, expected: f32, eps: f32, label: &str) {
  let delta = (actual - expected).abs();
  assert!(
    delta <= eps,
    "{label}: expected ~{expected} got {actual} (delta {delta}, eps {eps})"
  );
}

#[test]
fn viewport_changed_updates_frame_dpr_and_pixmap_size() {
  let _lock = super::stage_listener_test_lock();
  let handle = spawn_ui_worker("fastr-ui-worker-dpr-test-a").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId(1);
  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("send CreateTab");
  ui_tx
    .send(navigate_msg(
      tab_id,
      "about:newtab".to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("send Navigate");

  // Drain the initial frame rendered as part of navigation.
  let _ = recv_frame_ready(&ui_rx, tab_id, FRAME_TIMEOUT);

  ui_tx
    .send(viewport_changed_msg(tab_id, (100, 50), 1.0))
    .expect("send ViewportChanged 1.0");
  let frame1 = recv_frame_ready(&ui_rx, tab_id, FRAME_TIMEOUT);
  assert_eq!(frame1.viewport_css, (100, 50));
  assert_close(frame1.dpr, 1.0, 0.01, "frame1.dpr");
  let (w1, h1) = (frame1.pixmap.width(), frame1.pixmap.height());
  assert!(w1 > 0 && h1 > 0, "expected non-zero pixmap; got {w1}x{h1}");

  ui_tx
    .send(viewport_changed_msg(tab_id, (100, 50), 2.0))
    .expect("send ViewportChanged 2.0");
  let frame2 = recv_frame_ready(&ui_rx, tab_id, FRAME_TIMEOUT);
  assert_eq!(frame2.viewport_css, (100, 50));
  assert_close(frame2.dpr, 2.0, 0.01, "frame2.dpr");
  let (w2, h2) = (frame2.pixmap.width(), frame2.pixmap.height());

  assert_close(w2 as f32 / w1 as f32, 2.0, 0.1, "pixmap width ratio");
  assert_close(h2 as f32 / h1 as f32, 2.0, 0.1, "pixmap height ratio");

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn invalid_dpr_is_sanitized() {
  let _lock = super::stage_listener_test_lock();
  let handle = spawn_ui_worker("fastr-ui-worker-dpr-test-b").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId(1);
  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("send CreateTab");
  ui_tx
    .send(navigate_msg(
      tab_id,
      "about:newtab".to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("send Navigate");

  // Drain the initial frame rendered as part of navigation.
  let _ = recv_frame_ready(&ui_rx, tab_id, FRAME_TIMEOUT);

  ui_tx
    .send(viewport_changed_msg(tab_id, (100, 50), 0.0))
    .expect("send ViewportChanged invalid dpr");
  let frame = recv_frame_ready(&ui_rx, tab_id, FRAME_TIMEOUT);
  assert_eq!(frame.viewport_css, (100, 50));
  assert!(
    frame.dpr.is_finite() && frame.dpr >= 0.1,
    "expected sanitized dpr (>=0.1, finite), got {}",
    frame.dpr
  );
  assert!(
    frame.pixmap.width() > 0 && frame.pixmap.height() > 0,
    "expected non-zero pixmap for invalid dpr; got {}x{}",
    frame.pixmap.width(),
    frame.pixmap.height()
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
