#![cfg(feature = "browser_ui")]

use super::support::{
  create_tab_msg, navigate_msg, request_repaint, scroll_msg, viewport_changed_msg, DEFAULT_TIMEOUT,
};
use super::worker_harness::{WorkerHarness, WorkerToUiEvent};
use fastrender::ui::messages::{NavigationReason, RepaintReason, TabId, UiToWorker};

#[test]
fn tab_switching_preserves_per_tab_scroll_position() {
  let _lock = super::stage_listener_test_lock();
  let h = WorkerHarness::spawn();

  let viewport = (240, 180);
  let dpr = 1.0;

  // ---------------------------------------------------------------------------
  // Tab A: navigate to a scrollable page and scroll down.
  // ---------------------------------------------------------------------------
  let tab_a = TabId::new();
  h.send(create_tab_msg(tab_a, None));
  h.send(viewport_changed_msg(tab_a, viewport, dpr));
  h.send(navigate_msg(
    tab_a,
    "about:test-scroll".to_string(),
    NavigationReason::TypedUrl,
  ));

  h.wait_for_event(DEFAULT_TIMEOUT, |ev| {
    matches!(
      ev,
      WorkerToUiEvent::NavigationCommitted { tab_id, url, .. }
        if *tab_id == tab_a && url == "about:test-scroll"
    )
  });
  let (frame_a, _events) = h.wait_for_frame(tab_a, DEFAULT_TIMEOUT);
  assert_eq!(
    frame_a.viewport_css, viewport,
    "expected initial tab A frame to use requested viewport"
  );

  // Drain the post-navigation `ScrollStateUpdated` so subsequent waits only observe the scroll
  // message we send below.
  h.wait_for_event(DEFAULT_TIMEOUT, |ev| {
    matches!(ev, WorkerToUiEvent::ScrollStateUpdated { tab_id, .. } if *tab_id == tab_a)
  });

  h.send(scroll_msg(tab_a, (0.0, 300.0), None));
  let (scrolled_frame_a, _events) = h.wait_for_frame(tab_a, DEFAULT_TIMEOUT);
  let scrolled_y = scrolled_frame_a.scroll_state.viewport.y;
  assert!(
    scrolled_y > 0.0,
    "expected scroll to move tab A viewport down, got y={scrolled_y}"
  );

  let scroll_update_events = h.wait_for_event(DEFAULT_TIMEOUT, |ev| {
    matches!(ev, WorkerToUiEvent::ScrollStateUpdated { tab_id, .. } if *tab_id == tab_a)
  });
  let updated_scroll_y = match scroll_update_events
    .last()
    .expect("wait_for_event returns at least one event")
  {
    WorkerToUiEvent::ScrollStateUpdated { scroll, .. } => scroll.viewport.y,
    other => panic!("expected ScrollStateUpdated, got {other:?}"),
  };
  assert!(
    (updated_scroll_y - scrolled_y).abs() < 1e-3,
    "expected ScrollStateUpdated y to match FrameReady y (frame={scrolled_y}, update={updated_scroll_y})"
  );

  // ---------------------------------------------------------------------------
  // Tab B: create and navigate to a different page.
  // ---------------------------------------------------------------------------
  let tab_b = TabId::new();
  h.send(create_tab_msg(tab_b, None));
  h.send(viewport_changed_msg(tab_b, viewport, dpr));
  h.send(navigate_msg(
    tab_b,
    "about:blank".to_string(),
    NavigationReason::TypedUrl,
  ));

  h.wait_for_event(DEFAULT_TIMEOUT, |ev| {
    matches!(
      ev,
      WorkerToUiEvent::NavigationCommitted { tab_id, url, .. }
        if *tab_id == tab_b && url == "about:blank"
    )
  });
  let (_frame_b, _events) = h.wait_for_frame(tab_b, DEFAULT_TIMEOUT);
  // Drain the post-navigation ScrollStateUpdated for tab B.
  h.wait_for_event(DEFAULT_TIMEOUT, |ev| {
    matches!(ev, WorkerToUiEvent::ScrollStateUpdated { tab_id, .. } if *tab_id == tab_b)
  });

  // ---------------------------------------------------------------------------
  // Switch active tab B → A and ensure tab A's scroll position is preserved.
  // ---------------------------------------------------------------------------
  h.send(UiToWorker::SetActiveTab { tab_id: tab_b });
  h.send(request_repaint(tab_b, RepaintReason::Explicit));
  let (_frame_b_repaint, _events) = h.wait_for_frame(tab_b, DEFAULT_TIMEOUT);
  h.wait_for_event(DEFAULT_TIMEOUT, |ev| {
    matches!(ev, WorkerToUiEvent::ScrollStateUpdated { tab_id, .. } if *tab_id == tab_b)
  });

  h.send(UiToWorker::SetActiveTab { tab_id: tab_a });
  h.send(request_repaint(tab_a, RepaintReason::Explicit));
  let (frame_a_after_switch, _events) = h.wait_for_frame(tab_a, DEFAULT_TIMEOUT);
  let restored_y = frame_a_after_switch.scroll_state.viewport.y;

  assert!(
    (restored_y - scrolled_y).abs() < 1e-3,
    "expected tab A scroll_y to be preserved across tab switching (before={scrolled_y}, after={restored_y})"
  );
}
