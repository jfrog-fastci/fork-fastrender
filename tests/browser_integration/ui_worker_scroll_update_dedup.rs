#![cfg(feature = "browser_ui")]

use super::support::{
  create_tab_msg, navigate_msg, scroll_viewport, viewport_changed_msg, TempSite, DEFAULT_TIMEOUT,
};
use super::worker_harness::{WorkerHarness, WorkerToUiEvent};
use fastrender::ui::messages::{NavigationReason, TabId};
use std::time::{Duration, Instant};

fn create_tab(h: &WorkerHarness, viewport: (u32, u32)) -> TabId {
  let tab_id = TabId::new();
  // The canonical UI worker does not auto-navigate when `initial_url` is `None`. These tests
  // exercise interactions against a live document, so start tabs at `about:newtab` explicitly.
  h.send(create_tab_msg(tab_id, Some("about:newtab".to_string())));
  h.send(viewport_changed_msg(tab_id, viewport, 1.0));

  // `ViewportChanged` can race with the initial navigation; wait until we observe a frame at the
  // desired dimensions so subsequent scroll assertions are deterministic.
  let deadline = Instant::now() + Duration::from_secs(10);
  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    assert!(!remaining.is_zero(), "timed out waiting for initial frame at {viewport:?}");
    let (frame, events) = h.wait_for_frame(tab_id, remaining);
    // Drain straggler messages (e.g. ScrollStateUpdated) so the next action's wait only observes its
    // own effects.
    let _ = drain_after_frame(h, events);
    if frame.viewport_css == viewport {
      break;
    }
  }
  tab_id
}

fn drain_after_frame(h: &WorkerHarness, mut events: Vec<WorkerToUiEvent>) -> Vec<WorkerToUiEvent> {
  events.extend(h.drain_events(Duration::from_millis(200)));
  events
}

#[test]
fn scroll_state_updated_is_deduped_for_single_scroll_action() {
  let h = WorkerHarness::spawn();
  let tab_id = create_tab(&h, (120, 120));

  let site = TempSite::new();
  let url = site.write(
    "scroll.html",
    r#"<!doctype html>
      <style>
        html, body { margin: 0; padding: 0; }
        #spacer { height: 2000px; }
      </style>
      <div id="spacer"></div>
    "#,
  );

  let (_frame, events) =
    h.send_and_wait_for_frame(tab_id, navigate_msg(tab_id, url, NavigationReason::TypedUrl));
  let _ = drain_after_frame(&h, events);

  // Ensure the receive queue is empty before we start collecting scroll updates.
  let _ = h.drain_events(Duration::from_millis(100));

  h.send(scroll_viewport(tab_id, (0.0, 120.0)));
  let (frame, events) = h.wait_for_frame(tab_id, DEFAULT_TIMEOUT);
  let events = drain_after_frame(&h, events);

  assert!(
    frame.scroll_state.viewport.y > 0.0,
    "expected scroll action to change painted scroll offset, got {:?}",
    frame.scroll_state.viewport
  );

  let scroll_updates: Vec<_> = events
    .iter()
    .filter_map(|ev| match ev {
      WorkerToUiEvent::ScrollStateUpdated { tab_id: got, scroll } if *got == tab_id => {
        Some(scroll.clone())
      }
      _ => None,
    })
    .collect();

  assert!(
    !scroll_updates.is_empty(),
    "expected at least one ScrollStateUpdated when scroll changes; events={events:?}"
  );

  let expected_viewport = frame.scroll_state.viewport;
  let matching_final = scroll_updates
    .iter()
    .filter(|scroll| scroll.viewport == expected_viewport)
    .count();

  assert_eq!(
    matching_final, 1,
    "expected exactly one ScrollStateUpdated matching the painted scroll position; got {matching_final} (updates={scroll_updates:?}, expected_viewport={expected_viewport:?})"
  );
}
