#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::render_control::StageHeartbeat;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{NavigationReason, RepaintReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::render_worker::{
  reset_scroll_blit_stats_for_test, scroll_blit_disabled_due_to_animation_time_count_for_test,
  scroll_blit_used_count_for_test,
};
use fastrender::ui::spawn_ui_worker_for_test;
use std::time::Duration;

// Keep waits responsive even on contended CI hosts.
const WAIT_SLICE: Duration = Duration::from_millis(25);

fn animated_scroll_page() -> String {
  // The animation is infinite, so the initial (no-tick) frame resolves to the underlying style
  // (`background: rgb(0, 0, 0)`). Once `UiToWorker::Tick` sets `animation_time`, the element should
  // paint a different color.
  //
  // Place the animated box such that it remains within the *overlapping* region after a scroll,
  // so a scroll-blit fast path would incorrectly reuse stale pixels if it ignores animation time.
  r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255, 255, 255); }
      #anim {
        width: 200px;
        height: 200px;
        margin-top: 50px;
        background: rgb(0, 0, 0);
        animation: flash 32ms linear infinite;
      }
      @keyframes flash {
        from { background: rgb(0, 0, 0); }
        to { background: rgb(255, 255, 255); }
      }
      #spacer { height: 2000px; }
    </style>
  </head>
  <body>
    <div id="anim"></div>
    <div id="spacer"></div>
  </body>
</html>"#
    .to_string()
}

fn wait_for_navigation_commit_and_frame(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
  timeout: Duration,
) -> fastrender::ui::messages::RenderedFrame {
  let deadline = std::time::Instant::now() + timeout;
  let mut committed = false;
  let mut last_frame: Option<fastrender::ui::messages::RenderedFrame> = None;
  let mut messages: Vec<WorkerToUi> = Vec::new();

  while std::time::Instant::now() < deadline {
    match rx.recv_timeout(WAIT_SLICE) {
      Ok(msg) => {
        match &msg {
          WorkerToUi::NavigationCommitted { tab_id: msg_id, .. } if *msg_id == tab_id => {
            committed = true;
          }
          WorkerToUi::NavigationFailed {
            tab_id: msg_id,
            url,
            error,
            ..
          } if *msg_id == tab_id => {
            panic!("navigation failed for {url}: {error}");
          }
          _ => {}
        }

        match msg {
          WorkerToUi::FrameReady { tab_id: msg_id, frame } if msg_id == tab_id => {
            last_frame = Some(frame);
          }
          other => messages.push(other),
        }

        if committed && last_frame.is_some() {
          return last_frame.unwrap();
        }
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  panic!(
    "timed out waiting for NavigationCommitted + FrameReady; got:\n{}",
    support::format_messages(&messages)
  );
}

#[test]
fn scroll_blit_is_disabled_when_animation_time_advanced_since_last_paint() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  reset_scroll_blit_stats_for_test();

  let site = support::TempSite::new();
  let url = site.write("index.html", &animated_scroll_page());

  // Slow down deadline checks in the worker thread so the tick repaint stays in-flight long enough
  // for the test to cancel it.
  let handle =
    spawn_ui_worker_for_test("fastr-ui-worker-scroll-blit-animation-time", Some(2))
      .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let cancel = CancelGens::new();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg_with_cancel(
      tab_id,
      None,
      cancel.clone(),
    ))
    .expect("CreateTab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("Navigate");

  let _initial_frame = wait_for_navigation_commit_and_frame(&ui_rx, tab_id, support::DEFAULT_TIMEOUT);

  // Drain any straggler stage heartbeats so the next paint-stage marker we observe corresponds to
  // the tick repaint.
  while ui_rx.recv_timeout(Duration::from_millis(50)).is_ok() {}

  // Trigger an animation tick repaint.
  ui_tx
    .send(UiToWorker::Tick {
      tab_id,
      delta: Duration::from_millis(16),
    })
    .expect("send Tick");

  // Wait for the tick repaint to begin (paint-stage heartbeat) before cancelling it so we cancel
  // an in-flight job rather than a queued tick.
  let mut messages: Vec<WorkerToUi> = Vec::new();
  let mut saw_paint_stage = false;
  while !saw_paint_stage {
    match ui_rx.recv_timeout(Duration::from_secs(10)) {
      Ok(msg) => {
        if matches!(
          &msg,
          WorkerToUi::Stage {
            tab_id: msg_id,
            stage: StageHeartbeat::PaintBuild | StageHeartbeat::PaintRasterize
          } if *msg_id == tab_id
        ) {
          saw_paint_stage = true;
        }
        messages.push(msg);
      }
      Err(err) => panic!("timed out waiting for tick paint stage heartbeat: {err}"),
    }
  }

  // Cancel the in-flight tick repaint so the tab's animation time advances but the last painted
  // frame remains at the old animation sampling time.
  cancel.bump_paint();

  // Scroll while the tick repaint is being cancelled. A scroll-blit fast path would normally be a
  // candidate here, but must be disabled because animation time advanced since the last painted
  // frame.
  ui_tx
    .send(support::scroll_msg(tab_id, (0.0, 40.0), None))
    .expect("send Scroll");

  // Wait for the first frame that reflects the scroll (y > 0).
  let deadline = std::time::Instant::now() + support::DEFAULT_TIMEOUT;
  let mut scrolled_frame: Option<fastrender::ui::messages::RenderedFrame> = None;
  while std::time::Instant::now() < deadline {
    match ui_rx.recv_timeout(WAIT_SLICE) {
      Ok(msg) => match msg {
        WorkerToUi::FrameReady { tab_id: msg_id, frame } if msg_id == tab_id => {
          if frame.scroll_state.viewport.y > 0.0 {
            scrolled_frame = Some(frame);
            break;
          }
          messages.push(WorkerToUi::FrameReady {
            tab_id: msg_id,
            frame,
          });
        }
        other => messages.push(other),
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  let scrolled_frame = scrolled_frame.unwrap_or_else(|| {
    panic!(
      "timed out waiting for scrolled FrameReady; got:\n{}",
      support::format_messages(&messages)
    )
  });

  assert_eq!(
    scroll_blit_used_count_for_test(),
    0,
    "scroll blit should not be used when animation time changed",
  );
  assert!(
    scroll_blit_disabled_due_to_animation_time_count_for_test() >= 1,
    "expected scroll blit to be disabled due to animation time advancing",
  );

  // Force a full repaint at the current scroll position and compare output bytes. When scroll blit
  // is (incorrectly) used after a tick, the overlapping region would reuse stale pixels and the
  // output would differ from a full repaint reference.
  let expected_scroll_y = scrolled_frame.scroll_state.viewport.y;
  ui_tx
    .send(support::request_repaint(tab_id, RepaintReason::Explicit))
    .expect("RequestRepaint");

  let deadline = std::time::Instant::now() + support::DEFAULT_TIMEOUT;
  let mut full_repaint: Option<fastrender::ui::messages::RenderedFrame> = None;
  while std::time::Instant::now() < deadline {
    match ui_rx.recv_timeout(WAIT_SLICE) {
      Ok(msg) => match msg {
        WorkerToUi::FrameReady { tab_id: msg_id, frame } if msg_id == tab_id => {
          if (frame.scroll_state.viewport.y - expected_scroll_y).abs() < 1e-3 {
            full_repaint = Some(frame);
            break;
          }
          messages.push(WorkerToUi::FrameReady {
            tab_id: msg_id,
            frame,
          });
        }
        other => messages.push(other),
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  let full_repaint = full_repaint.unwrap_or_else(|| {
    panic!(
      "timed out waiting for full repaint FrameReady; got:\n{}",
      support::format_messages(&messages)
    )
  });

  assert_eq!(
    scrolled_frame.pixmap.data(),
    full_repaint.pixmap.data(),
    "scrolled frame should match a forced full repaint when animation time advanced",
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
