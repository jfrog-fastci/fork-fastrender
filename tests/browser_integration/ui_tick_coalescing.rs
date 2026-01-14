#![cfg(feature = "browser_ui")]

use super::support::{
  create_tab_msg, navigate_msg, request_repaint, viewport_changed_msg, TempSite, DEFAULT_TIMEOUT,
};
use super::worker_harness::{WorkerHarness, WorkerToUiEvent};
use fastrender::render_control::StageHeartbeat;
use fastrender::ui::messages::{NavigationReason, RepaintReason, TabId, UiToWorker};
use fastrender::ui::render_worker::{
  disable_tick_stats_for_test, reset_tick_stats_for_test, tick_delta_total_for_test,
  tick_handle_count_for_test,
};
use std::time::{Duration, Instant};

struct TickStatsGuard;

impl Drop for TickStatsGuard {
  fn drop(&mut self) {
    disable_tick_stats_for_test();
  }
}

#[test]
fn tick_burst_coalesces_in_worker_runtime() {
  let _lock = super::stage_listener_test_lock();

  reset_tick_stats_for_test();
  let _tick_stats_guard = TickStatsGuard;

  // Slow down paints so we can deterministically enqueue a burst of Tick messages while the worker
  // runtime thread is busy painting.
  let h = WorkerHarness::spawn_with_test_render_delay(Some(50));

  let tab_id = TabId::new();
  h.send(create_tab_msg(tab_id, None));
  h.send(viewport_changed_msg(tab_id, (64, 64), 1.0));

  let site = TempSite::new();
  let url = site.write(
    "anim.html",
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body { margin: 0; padding: 0; }
            #box {
              width: 64px;
              height: 64px;
              background: rgb(255, 0, 0);
              animation: fade 100ms linear infinite;
            }
            @keyframes fade {
              from { opacity: 0; }
              to { opacity: 1; }
            }
          </style>
        </head>
        <body><div id="box"></div></body>
      </html>"#,
  );

  h.send(navigate_msg(tab_id, url, NavigationReason::TypedUrl));
  h.wait_for_event(DEFAULT_TIMEOUT, |ev| {
    matches!(
      ev,
      WorkerToUiEvent::NavigationCommitted { tab_id: id, .. } if *id == tab_id
    )
  });

  let (frame, _events) = h.wait_for_frame(tab_id, DEFAULT_TIMEOUT);
  assert!(
    frame.next_tick.is_some(),
    "expected animation fixture to request ticks"
  );

  // Ensure the worker is in the middle of a paint so ticks will queue up behind it.
  h.send(request_repaint(tab_id, RepaintReason::Explicit));
  h.wait_for_event(DEFAULT_TIMEOUT, |ev| {
    matches!(
      ev,
      WorkerToUiEvent::Stage { tab_id: id, stage }
        if *id == tab_id
          && matches!(stage, StageHeartbeat::PaintBuild | StageHeartbeat::PaintRasterize)
    )
  });

  const TICK_COUNT: usize = 10;
  let tick_delta = Duration::from_millis(16);
  let expected_total = Duration::from_millis(TICK_COUNT as u64 * 16);

  // Send ticks spaced just beyond the router coalescing window (4ms) so they enqueue into the
  // runtime thread's channel and must be coalesced by `BrowserRuntime::drain_messages`.
  for _ in 0..TICK_COUNT {
    h.send(UiToWorker::Tick {
      tab_id,
      delta: tick_delta,
    });
    std::thread::sleep(Duration::from_millis(5));
  }

  // Wait for the worker to process the tick burst (tick stats are updated in `handle_tick`).
  let deadline = Instant::now() + DEFAULT_TIMEOUT;
  loop {
    let got = tick_delta_total_for_test(tab_id);
    if got >= expected_total {
      break;
    }
    if Instant::now() >= deadline {
      panic!(
        "timed out waiting for worker to process tick burst; expected {:?}, got {:?}",
        expected_total, got
      );
    }
    std::thread::sleep(Duration::from_millis(10));
  }

  assert_eq!(
    tick_delta_total_for_test(tab_id),
    expected_total,
    "expected worker to advance tick time by the total delta of the burst"
  );

  let handle_count = tick_handle_count_for_test(tab_id);
  assert!(
    handle_count <= 2,
    "expected tick burst to be coalesced; sent {TICK_COUNT} ticks, worker handled {handle_count}"
  );

  // Best-effort frame-count assertion: we should not emit a frame per tick when the worker can't
  // keep up.
  let events = h.drain_events(Duration::from_millis(500));
  let frames = events
    .iter()
    .filter(|ev| matches!(ev, WorkerToUiEvent::FrameReady { tab_id: id, .. } if *id == tab_id))
    .count();
  assert!(
    frames <= 2,
    "expected tick burst not to emit many frames; got {frames} frames; events={events:?}"
  );
}

#[test]
fn tick_delta_is_clamped_in_worker() {
  let _lock = super::stage_listener_test_lock();

  reset_tick_stats_for_test();
  let _tick_stats_guard = TickStatsGuard;

  let h = WorkerHarness::spawn_with_test_render_delay(None);
  let tab_id = TabId::new();
  h.send(create_tab_msg(tab_id, None));

  h.send(UiToWorker::Tick {
    tab_id,
    delta: Duration::from_secs(10),
  });

  let expected = Duration::from_secs(1);
  let deadline = Instant::now() + DEFAULT_TIMEOUT;
  loop {
    let got = tick_delta_total_for_test(tab_id);
    if got >= expected {
      break;
    }
    if Instant::now() >= deadline {
      panic!(
        "timed out waiting for worker to process tick; expected {:?}, got {:?}",
        expected, got
      );
    }
    std::thread::sleep(Duration::from_millis(10));
  }

  assert_eq!(tick_handle_count_for_test(tab_id), 1);
  assert_eq!(tick_delta_total_for_test(tab_id), expected);
}
