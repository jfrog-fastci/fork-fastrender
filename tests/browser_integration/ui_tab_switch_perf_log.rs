#![cfg(feature = "browser_ui")]

use super::support::{create_tab, request_repaint, viewport_changed_msg, DEFAULT_TIMEOUT};
use fastrender::ui::messages::{RepaintReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::{BrowserAppState, BrowserTabState, FrameUploadCoalescer};
use std::collections::HashSet;
use std::time::{Duration, Instant};

#[test]
fn tab_switch_perf_log_reports_cached_when_background_frame_uploaded() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();

  // Enable perf logging for this process.
  let _perf_log_env = crate::common::EnvVarGuard::set("FASTR_PERF_LOG", "1");
  // Clear any previous events (best-effort; should be empty for most test runs).
  let _ = fastrender::ui::perf_log::drain_perf_log_events();

  let handle =
    fastrender::ui::spawn_ui_worker("fastr-ui-tab-switch-perf-log-test").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_a = TabId::new();
  let tab_b = TabId::new();

  // UI-side model state: tab A is active.
  let mut app_state = BrowserAppState::new();
  app_state.push_tab(BrowserTabState::new(tab_a, "about:blank".to_string()), true);
  app_state.push_tab(BrowserTabState::new(tab_b, "about:newtab".to_string()), false);
  assert_eq!(app_state.active_tab_id(), Some(tab_a));

  // Simulated UI-side texture registry: if an upload is scheduled for a tab, that tab's texture
  // would be created/updated in the windowed UI.
  let mut textures: HashSet<TabId> = HashSet::new();
  let mut pending_uploads = FrameUploadCoalescer::new();

  // Worker setup: create both tabs and render them at a small viewport.
  ui_tx
    .send(create_tab(tab_a, Some("about:blank")))
    .expect("create tab A");
  ui_tx
    .send(create_tab(tab_b, Some("about:newtab")))
    .expect("create tab B");
  ui_tx
    .send(viewport_changed_msg(tab_a, (64, 64), 1.0))
    .expect("viewport A");
  ui_tx
    .send(viewport_changed_msg(tab_b, (64, 64), 1.0))
    .expect("viewport B");
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id: tab_a })
    .expect("set active A");
  ui_tx
    .send(request_repaint(tab_a, RepaintReason::Explicit))
    .expect("repaint A");
  ui_tx
    .send(request_repaint(tab_b, RepaintReason::Explicit))
    .expect("repaint B");

  // Collect the first frames for both tabs.
  let deadline = Instant::now() + DEFAULT_TIMEOUT;
  let mut frame_a = None;
  let mut frame_b = None;
  let mut seen_msgs: Vec<WorkerToUi> = Vec::new();

  while Instant::now() < deadline && (frame_a.is_none() || frame_b.is_none()) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let slice = remaining.min(Duration::from_millis(50));
    match ui_rx.recv_timeout(slice) {
      Ok(msg) => {
        if let WorkerToUi::FrameReady { tab_id, frame } = msg {
          if tab_id == tab_a && frame_a.is_none() {
            frame_a = Some(frame);
          } else if tab_id == tab_b && frame_b.is_none() {
            frame_b = Some(frame);
          }
        } else {
          seen_msgs.push(msg);
        }
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => panic!("worker disconnected"),
    }
  }

  let frame_a = frame_a
    .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab A; msgs={seen_msgs:?}"));
  let frame_b = frame_b
    .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab B; msgs={seen_msgs:?}"));

  // Feed both frames into the UI state model + upload planner, flushing after each to simulate the
  // browser UI's "coalesce then upload" behavior.
  for (tab_id, frame) in [(tab_a, frame_a), (tab_b, frame_b)] {
    let update = app_state.apply_worker_msg(WorkerToUi::FrameReady { tab_id, frame });
    if let Some(frame_ready) = update.frame_ready {
      pending_uploads.push(frame_ready);
    }
    for frame_ready in pending_uploads.drain() {
      textures.insert(frame_ready.tab_id);
    }
  }

  assert!(
    textures.contains(&tab_b),
    "expected background tab B to have an uploaded texture before switching"
  );

  // Simulate a user-initiated switch to tab B.
  let mut tracker = fastrender::ui::perf_log::TabSwitchLatencyTracker::new();
  let prev_active = app_state.active_tab_id();
  assert!(
    app_state.set_active_tab(tab_b),
    "expected BrowserAppState to accept activating tab B"
  );
  let Some(prev_active) = prev_active else {
    panic!("expected a previous active tab");
  };

  // Match the windowed UI's cache heuristic: either an uploaded texture or UI-side latest-frame
  // metadata is considered cached.
  let cached = textures.contains(&tab_b)
    || app_state
      .tab(tab_b)
      .and_then(|tab| tab.latest_frame_meta.as_ref())
      .is_some();
  tracker.start(prev_active, tab_b, cached);

  // The tab is immediately presentable because its texture is already uploaded.
  tracker.mark_tab_presented(tab_b);

  let events = fastrender::ui::perf_log::drain_perf_log_events();
  let tab_switch_event = events.iter().find_map(|event| match event {
    fastrender::ui::perf_log::PerfLogEvent::TabSwitch {
      from_tab,
      to_tab,
      cached,
      ..
    } => Some((*from_tab, *to_tab, *cached)),
  });

  let (from_tab, to_tab, cached) =
    tab_switch_event.unwrap_or_else(|| panic!("expected tab_switch event, got {events:?}"));
  assert_eq!(from_tab, prev_active.0);
  assert_eq!(to_tab, tab_b.0);
  assert!(cached, "expected cached=true for background frame already uploaded");

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
