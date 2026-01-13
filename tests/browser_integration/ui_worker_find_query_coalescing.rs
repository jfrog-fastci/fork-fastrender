#![cfg(feature = "browser_ui")]

use super::support::{
  create_tab_msg, navigate_msg, request_repaint, viewport_changed_msg, TempSite, DEFAULT_TIMEOUT,
};
use super::worker_harness::{WorkerHarness, WorkerToUiEvent};
use fastrender::ui::messages::{NavigationReason, RepaintReason, TabId, UiToWorker};
use std::time::Duration;

#[test]
fn find_query_burst_coalesces_to_latest_query() {
  let _lock = super::stage_listener_test_lock();

  // Slow down paints so we can deterministically enqueue a burst of FindQuery messages while the
  // worker is busy, ensuring they are drained/coalesced in one go.
  let h = WorkerHarness::spawn_with_test_render_delay(Some(1));

  let viewport = (240, 160);
  let dpr = 1.0;

  let tab_id = TabId::new();
  h.send(create_tab_msg(tab_id, None));
  h.send(viewport_changed_msg(tab_id, viewport, dpr));

  let site = TempSite::new();
  let mut body = String::new();
  for i in 0..64 {
    body.push_str(&format!("<div>needle {i}</div>"));
  }
  let url = site.write(
    "index.html",
    &format!("<!doctype html><meta charset=utf-8><title>find</title><body>{body}</body>"),
  );

  h.send(navigate_msg(tab_id, url, NavigationReason::TypedUrl));
  h.wait_for_event(DEFAULT_TIMEOUT, |ev| {
    matches!(ev, WorkerToUiEvent::NavigationCommitted { tab_id: id, .. } if *id == tab_id)
  });
  let (_frame, _events) = h.wait_for_frame(tab_id, DEFAULT_TIMEOUT);

  // Ensure the worker is busy painting while we enqueue the FindQuery burst.
  h.send(request_repaint(tab_id, RepaintReason::Explicit));

  let mut expected_query = String::new();
  let mut expected_case_sensitive = false;
  for i in 0..12u32 {
    let query = format!("needle {i}");
    let case_sensitive = i % 3 == 0;
    expected_query = query.clone();
    expected_case_sensitive = case_sensitive;
    h.send(UiToWorker::FindQuery {
      tab_id,
      query,
      case_sensitive,
    });
  }

  let mut events = h.wait_for_event(DEFAULT_TIMEOUT, |ev| {
    matches!(
      ev,
      WorkerToUiEvent::FindResult {
        tab_id: id,
        query,
        case_sensitive,
        ..
      } if *id == tab_id && query == &expected_query && *case_sensitive == expected_case_sensitive
    )
  });
  events.extend(h.drain_events(Duration::from_millis(200)));

  let find_results: Vec<(String, bool)> = events
    .iter()
    .filter_map(|ev| match ev {
      WorkerToUiEvent::FindResult {
        tab_id: id,
        query,
        case_sensitive,
        ..
      } if *id == tab_id => Some((query.clone(), *case_sensitive)),
      _ => None,
    })
    .collect();

  assert_eq!(
    find_results,
    vec![(expected_query.clone(), expected_case_sensitive)],
    "expected FindQuery burst to be coalesced to a single FindResult for the latest query; got {find_results:?}"
  );
}

