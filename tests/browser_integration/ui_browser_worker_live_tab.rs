#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{RenderedFrame, RepaintReason, WorkerToUi};
use fastrender::ui::{spawn_ui_worker_for_test, spawn_ui_worker_with_factory};
use fastrender::ui::{TabId, UiToWorker};
use std::time::Duration;

// Worker startup + navigation + rendering can take a few seconds under load when integration tests
// run in parallel on CI; keep this timeout generous to avoid flakiness.
const TIMEOUT: Duration = Duration::from_secs(20);

fn next_frame(rx: &fastrender::ui::WorkerToUiInbox, tab_id: TabId) -> RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));
  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

#[test]
fn tick_unknown_tab_is_noop() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-tick-unknown",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let tab_id = TabId::new();

  handle
    .ui_tx
    .send(UiToWorker::Tick {
      tab_id,
      delta: Duration::from_millis(16),
    })
    .expect("tick");

  let msgs = support::drain_for(&handle.ui_rx, Duration::from_millis(200));
  assert!(
    msgs.is_empty(),
    "expected no messages after ticking an unknown tab, got:\n{}",
    support::format_messages(&msgs)
  );

  handle.join().expect("worker join");
}

#[test]
fn tick_does_not_repaint_clean_tab() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-tick-clean",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let tab_id = TabId::new();

  handle
    .ui_tx
    .send(support::create_tab_msg(
      tab_id,
      Some("about:blank".to_string()),
    ))
    .expect("create tab");
  handle
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (32, 32), 1.0))
    .expect("viewport");

  let initial = next_frame(&handle.ui_rx, tab_id);
  assert!(
    initial.next_tick.is_none(),
    "expected about:blank to render without time-based effects"
  );
  while handle.ui_rx.try_recv().is_ok() {}

  handle
    .ui_tx
    .send(UiToWorker::Tick {
      tab_id,
      delta: Duration::from_millis(16),
    })
    .expect("tick");

  let msgs = support::drain_for(&handle.ui_rx, Duration::from_millis(200));
  assert!(
    !msgs
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::FrameReady { .. })),
    "expected no FrameReady after tick on a clean tab, got:\n{}",
    support::format_messages(&msgs)
  );

  handle.join().expect("worker join");
}

#[test]
fn tick_does_not_schedule_for_unused_keyframes() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            html, body { background: rgb(0, 0, 0); }
            #box {
              width: 64px;
              height: 64px;
              background: rgb(255, 0, 0);
            }
            /* Keyframes are defined but never referenced by `animation-name`. */
            @keyframes fade {
              from { opacity: 0; }
              to { opacity: 1; }
            }
          </style>
        </head>
        <body>
          <div id="box"></div>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-tick-unused-keyframes",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let tab_id = TabId::new();

  handle
    .ui_tx
    .send(support::create_tab_msg(tab_id, Some(url)))
    .expect("create tab");
  handle
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .expect("viewport");

  let initial = next_frame(&handle.ui_rx, tab_id);
  assert!(
    initial.next_tick.is_none(),
    "expected unused @keyframes to not request periodic ticks"
  );
  // A navigation/initial paint does not guarantee a standalone `ScrollStateUpdated`, so just drain
  // follow-up messages before asserting tick behaviour.
  let _ = support::drain_for(&handle.ui_rx, Duration::from_millis(200));

  handle
    .ui_tx
    .send(UiToWorker::Tick {
      tab_id,
      delta: Duration::from_millis(16),
    })
    .expect("tick");

  let msgs = support::drain_for(&handle.ui_rx, Duration::from_millis(200));
  assert!(
    !msgs
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::FrameReady { .. })),
    "expected no FrameReady after tick on an unused-keyframes tab, got:\n{}",
    support::format_messages(&msgs)
  );

  handle.join().expect("worker join");
}

#[test]
fn tick_emits_new_frames_for_css_animation() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            html, body { background: rgb(0, 0, 0); }
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
        <body>
          <div id="box"></div>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-tick-animation",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let tab_id = TabId::new();
  handle
    .ui_tx
    .send(support::create_tab_msg(tab_id, Some(url)))
    .expect("create tab");
  handle
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .expect("viewport");

  let initial = next_frame(&handle.ui_rx, tab_id);
  assert!(
    initial.next_tick.is_some(),
    "expected animation fixture page to request periodic ticks"
  );
  while handle.ui_rx.try_recv().is_ok() {}

  handle
    .ui_tx
    .send(UiToWorker::Tick {
      tab_id,
      delta: Duration::from_millis(16),
    })
    .expect("tick 1");
  let frame1 = next_frame(&handle.ui_rx, tab_id);
  let bytes1 = frame1.pixmap.data().to_vec();

  handle
    .ui_tx
    .send(UiToWorker::Tick {
      tab_id,
      delta: Duration::from_millis(16),
    })
    .expect("tick 2");
  let frame2 = next_frame(&handle.ui_rx, tab_id);
  let bytes2 = frame2.pixmap.data().to_vec();

  assert_ne!(
    bytes1, bytes2,
    "expected pixmap to change between tick-driven animation frames"
  );

  handle.join().expect("worker join");
}

#[test]
fn tick_runs_js_request_animation_frame_and_repaints() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; width: 100%; height: 100%; }
            html, body { background: rgb(0, 0, 0); }
          </style>
          <script>
            requestAnimationFrame(() => {
              // Use `setProperty` because our CSSStyleDeclaration shim only exposes a limited set of
              // named property accessors (but supports `setProperty` for arbitrary declarations).
              document.documentElement.style.setProperty('background-color', 'rgb(0,255,0)');
            });
          </script>
        </head>
        <body>raf</body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-tick-js-raf",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let tab_id = TabId::new();
  handle
    .ui_tx
    .send(support::create_tab_msg(tab_id, Some(url)))
    .expect("create tab");
  handle
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (32, 32), 1.0))
    .expect("viewport");

  let initial = next_frame(&handle.ui_rx, tab_id);
  assert!(
    initial.next_tick.is_some(),
    "expected JS pages to request ticks (JS timers/rAF)"
  );
  assert_eq!(
    support::rgba_at(&initial.pixmap, 0, 0),
    [0, 0, 0, 255],
    "expected initial frame to be black before rAF callback runs"
  );
  while handle.ui_rx.try_recv().is_ok() {}

  handle
    .ui_tx
    .send(UiToWorker::Tick {
      tab_id,
      delta: Duration::from_millis(16),
    })
    .expect("tick");
  let frame = next_frame(&handle.ui_rx, tab_id);
  assert_eq!(
    support::rgba_at(&frame.pixmap, 0, 0),
    [0, 255, 0, 255],
    "expected tick to run rAF and repaint the updated background"
  );

  handle.join().expect("worker join");
}

#[test]
fn js_timer_dom_mutation_affects_rendered_pixels() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; width: 100%; height: 100%; background: rgb(0, 0, 0); }
          </style>
          <script>
            // Schedule a timer from inside an rAF callback so it won't run during the worker's
            // post-navigation JS pump (which does not execute rAF). This ensures `UiToWorker::Tick`
            // is responsible for driving both the rAF callback and the timer task.
            requestAnimationFrame(() => {
              setTimeout(function () {
                // Use `setProperty` because our CSSStyleDeclaration shim only exposes a limited set of
                // named property accessors (but supports `setProperty` for arbitrary declarations).
                document.documentElement.style.setProperty('background-color', 'rgb(255,0,0)');
                document.body.style.setProperty('background-color', 'rgb(255,0,0)');
              }, 0);
            });
          </script>
        </head>
        <body></body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-js-dom-pixels",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let tab_id = TabId::new();
  handle
    .ui_tx
    .send(support::create_tab_msg(tab_id, Some(url)))
    .expect("create tab");
  handle
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (32, 32), 1.0))
    .expect("viewport");

  let initial = next_frame(&handle.ui_rx, tab_id);
  assert!(
    initial.next_tick.is_some(),
    "expected JS pages to request ticks (JS timers/rAF)"
  );
  let baseline = support::rgba_at(&initial.pixmap, 0, 0);
  assert_eq!(
    baseline,
    [0, 0, 0, 255],
    "expected initial frame to be black before JS timers run"
  );

  // Drain navigation follow-ups so we only consider repaint-driven frames below.
  //
  // The worker no longer guarantees a standalone `ScrollStateUpdated` after every `FrameReady`, so
  // avoid waiting for it with a long timeout. A short bounded drain keeps the test deterministic
  // without introducing multi-second delays.
  let _ = support::drain_for(&handle.ui_rx, Duration::from_millis(200));
  while handle.ui_rx.try_recv().is_ok() {}

  let mut last_sample = baseline;
  let mut saw_change = false;

  // Poll for the `setTimeout(..., 0)` callback to run and change the DOM, forcing repaints so we
  // observe the updated pixels once the render worker syncs the JS DOM into the renderer document.
  for _ in 0..20 {
    handle
      .ui_tx
      .send(UiToWorker::Tick {
        tab_id,
        delta: Duration::from_millis(16),
      })
      .expect("tick");
    handle
      .ui_tx
      .send(support::request_repaint(tab_id, RepaintReason::Explicit))
      .expect("request repaint");
    let frame = next_frame(&handle.ui_rx, tab_id);
    last_sample = support::rgba_at(&frame.pixmap, 0, 0);
    if last_sample != baseline {
      saw_change = true;
      break;
    }
  }

  assert!(
    saw_change,
    "expected JS timer mutation to affect rendered pixels; baseline={baseline:?} last={last_sample:?}"
  );
  assert_eq!(
    last_sample,
    [255, 0, 0, 255],
    "expected JS-set background to render as red"
  );

  handle.join().expect("worker join");
}

#[test]
fn tick_burst_coalesces_to_single_frame() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #box {
              width: 32px;
              height: 32px;
              background: rgb(255, 0, 0);
              animation: fade 100ms linear infinite;
            }
            @keyframes fade {
              from { opacity: 0; }
              to { opacity: 1; }
            }
          </style>
        </head>
        <body>
          <div id="box"></div>
        </body>
      </html>"#,
  );

  // Slow down paints so a burst of ticks can accumulate in the UI→worker channel.
  let handle = spawn_ui_worker_for_test("fastr-ui-worker-tick-burst", Some(10))
    .expect("spawn ui worker");
  let tab_id = TabId::new();
  handle
    .ui_tx
    .send(support::create_tab_msg(tab_id, Some(url)))
    .expect("create tab");
  handle
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .expect("viewport");

  let initial = next_frame(&handle.ui_rx, tab_id);
  assert!(
    initial.next_tick.is_some(),
    "expected animation fixture page to request periodic ticks"
  );
  while handle.ui_rx.try_recv().is_ok() {}

  // Fire a burst of ticks back-to-back.
  for _ in 0..50 {
    handle
      .ui_tx
      .send(UiToWorker::Tick {
        tab_id,
        delta: Duration::from_millis(16),
      })
      .expect("tick");
  }

  // Observe the tick-driven frame.
  let _tick_frame = next_frame(&handle.ui_rx, tab_id);

  // Ensure no additional frames were produced for intermediate ticks.
  let drained = support::drain_for(&handle.ui_rx, Duration::from_secs(1));
  let extra_frames = drained
    .iter()
    .filter(|msg| matches!(msg, WorkerToUi::FrameReady { .. }))
    .count();
  assert_eq!(
    extra_frames, 0,
    "expected a single coalesced FrameReady for tick burst, got {extra_frames} extra:\n{}",
    support::format_messages(&drained)
  );

  handle.join().expect("worker join");
}

#[test]
fn tick_stops_after_finite_css_animation_completes() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; background: rgb(0, 0, 0); }
            #box {
              width: 64px;
              height: 64px;
              background: rgb(255, 0, 0);
              animation: fade 50ms linear 1 forwards;
            }
            @keyframes fade {
              from { opacity: 0; }
              to { opacity: 1; }
            }
          </style>
        </head>
        <body>
          <div id="box"></div>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-tick-finite-animation",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let tab_id = TabId::new();
  handle
    .ui_tx
    .send(support::create_tab_msg(tab_id, Some(url)))
    .expect("create tab");
  handle
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .expect("viewport");

  let mut frame = next_frame(&handle.ui_rx, tab_id);
  assert!(
    frame.next_tick.is_some(),
    "expected finite animation fixture page to request periodic ticks initially"
  );
  while handle.ui_rx.try_recv().is_ok() {}

  for _ in 0..10 {
    if frame.next_tick.is_none() {
      break;
    }
    handle
      .ui_tx
      .send(UiToWorker::Tick {
        tab_id,
        delta: Duration::from_millis(16),
      })
      .expect("tick");
    frame = next_frame(&handle.ui_rx, tab_id);
  }

  assert!(
    frame.next_tick.is_none(),
    "expected finite animation to stop requesting ticks within a few frames"
  );

  while handle.ui_rx.try_recv().is_ok() {}
  handle
    .ui_tx
    .send(UiToWorker::Tick {
      tab_id,
      delta: Duration::from_millis(16),
    })
    .expect("tick after completion");
  let msgs = support::drain_for(&handle.ui_rx, Duration::from_millis(200));
  assert!(
    !msgs
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::FrameReady { .. })),
    "expected no FrameReady after tick once finite animation completed, got:\n{}",
    support::format_messages(&msgs)
  );

  handle.join().expect("worker join");
}

#[test]
fn scroll_driven_animation_does_not_request_periodic_ticks() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            body { height: 200vh; background: rgb(0, 0, 0); }
            #box {
              width: 64px;
              height: 64px;
              background: rgb(255, 0, 0);
              animation: fade 1s linear 1 both;
              animation-timeline: scroll(root);
            }
            @keyframes fade {
              from { opacity: 0; }
              to { opacity: 1; }
            }
          </style>
        </head>
        <body>
          <div id="box"></div>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-scroll-driven-animation",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let tab_id = TabId::new();
  handle
    .ui_tx
    .send(support::create_tab_msg(tab_id, Some(url)))
    .expect("create tab");
  handle
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .expect("viewport");

  let initial = next_frame(&handle.ui_rx, tab_id);
  assert!(
    initial.next_tick.is_none(),
    "expected scroll-driven animation page to not request periodic ticks when idle"
  );
  while handle.ui_rx.try_recv().is_ok() {}

  handle
    .ui_tx
    .send(UiToWorker::Tick {
      tab_id,
      delta: Duration::from_millis(16),
    })
    .expect("tick");
  let msgs = support::drain_for(&handle.ui_rx, Duration::from_millis(200));
  assert!(
    !msgs
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::FrameReady { .. })),
    "expected no FrameReady after tick on scroll-driven animation tab, got:\n{}",
    support::format_messages(&msgs)
  );

  handle.join().expect("worker join");
}
