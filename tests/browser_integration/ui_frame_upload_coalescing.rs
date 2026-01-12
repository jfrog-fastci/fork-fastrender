#![cfg(feature = "browser_ui")]

use super::support::{create_tab, request_repaint, viewport_changed_msg, DEFAULT_TIMEOUT};
use fastrender::ui::messages::{RepaintReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::{BrowserAppState, BrowserTabState, FrameUploadCoalescer};
use std::collections::HashSet;
use std::time::{Duration, Instant};

#[test]
fn inactive_tab_frames_do_not_schedule_uploads_until_activated() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let handle = fastrender::ui::spawn_ui_worker("fastr-ui-frame-upload-coalescing-test")
    .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_a = TabId::new();
  let tab_b = TabId::new();

  // UI-side model state: tab A is active.
  let mut app_state = BrowserAppState::new();
  app_state.push_tab(BrowserTabState::new(tab_a, "about:blank".to_string()), true);
  app_state.push_tab(
    BrowserTabState::new(tab_b, "about:newtab".to_string()),
    false,
  );
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

  // Collect the first frames for both tabs without dropping frames for the "other" tab.
  let deadline = Instant::now() + DEFAULT_TIMEOUT;
  let mut frame_a = None;
  let mut frame_b = None;
  let mut seen_msgs: Vec<WorkerToUi> = Vec::new();

  while Instant::now() < deadline && (frame_a.is_none() || frame_b.is_none()) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    // Use small slices so we keep `seen_msgs` useful for failure output.
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
      pending_uploads.push_for_active_tab(app_state.active_tab_id(), frame_ready);
    }

    for frame_ready in pending_uploads.drain() {
      textures.insert(frame_ready.tab_id);
    }
  }

  assert!(
    textures.contains(&tab_a),
    "expected active tab A to schedule an upload"
  );
  assert!(
    !textures.contains(&tab_b),
    "expected inactive tab B to not schedule an upload until activated"
  );

  assert!(
    app_state
      .tab(tab_b)
      .and_then(|t| t.latest_frame_meta.as_ref())
      .is_some(),
    "expected UI-side metadata to be updated for inactive tab B"
  );

  // Activate tab B: the UI requests a fresh repaint so a new frame is produced and uploaded.
  assert!(app_state.set_active_tab(tab_b));
  pending_uploads.clear();
  ui_tx
    .send(UiToWorker::SetActiveTab { tab_id: tab_b })
    .expect("set active B");
  ui_tx
    .send(request_repaint(tab_b, RepaintReason::Explicit))
    .expect("repaint B after activation");

  let deadline = Instant::now() + DEFAULT_TIMEOUT;
  let mut activated_frame_b = None;
  while Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let slice = remaining.min(Duration::from_millis(50));
    match ui_rx.recv_timeout(slice) {
      Ok(WorkerToUi::FrameReady { tab_id, frame }) if tab_id == tab_b => {
        activated_frame_b = Some(frame);
        break;
      }
      Ok(_) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => panic!("worker disconnected"),
    }
  }
  let activated_frame_b = activated_frame_b
    .unwrap_or_else(|| panic!("timed out waiting for FrameReady for activated tab B"));

  let update = app_state.apply_worker_msg(WorkerToUi::FrameReady {
    tab_id: tab_b,
    frame: activated_frame_b,
  });
  if let Some(frame_ready) = update.frame_ready {
    pending_uploads.push_for_active_tab(app_state.active_tab_id(), frame_ready);
  }

  for frame_ready in pending_uploads.drain() {
    textures.insert(frame_ready.tab_id);
  }

  assert!(
    textures.contains(&tab_b),
    "expected activated tab B to schedule an upload"
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
