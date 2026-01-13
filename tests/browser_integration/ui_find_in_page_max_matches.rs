#![cfg(feature = "browser_ui")]

use std::time::Duration;

use fastrender::ui::messages::{NavigationReason, RepaintReason, TabId, UiToWorker};

use super::support;
use super::worker_harness::{WorkerHarness, WorkerToUiEvent};

/// Keep in sync with `src/ui/find_in_page.rs:MAX_FIND_MATCHES`.
const MAX_FIND_MATCHES: usize = 10_000;

/// Build a page that contains far more matches than `MAX_FIND_MATCHES` without generating a huge DOM
/// tree (one `<pre>` text node).
fn build_pathological_find_page(total_matches: usize) -> String {
  // Build `a` tokens separated by spaces with periodic newlines. This keeps the HTML reasonably
  // small while ensuring the rendered fragment tree contains a large number of matches.
  const TOKENS_PER_LINE: usize = 200;

  let mut body = String::with_capacity(total_matches * 2);
  for i in 0..total_matches {
    body.push('a');
    if i % TOKENS_PER_LINE == TOKENS_PER_LINE - 1 {
      body.push('\n');
    } else {
      body.push(' ');
    }
  }

  let mut html = String::with_capacity(body.len() + 256);
  html.push_str("<!doctype html><meta charset=\"utf-8\">");
  html.push_str("<style>html,body{margin:0;padding:0;font:12px monospace}</style>");
  html.push_str("<pre>");
  html.push_str(&body);
  html.push_str("</pre>");
  html
}

#[test]
fn find_in_page_caps_match_count_and_worker_remains_responsive() {
  let harness = WorkerHarness::spawn();

  // Keep the viewport tiny so the test doesn't spend time allocating/painting large pixmaps.
  let tab_id = TabId(1);
  harness.send(support::create_tab_msg(tab_id, None));
  harness.send(support::viewport_changed_msg(tab_id, (64, 64), 1.0));

  let site = support::TempSite::new();
  let html = build_pathological_find_page(MAX_FIND_MATCHES * 5);
  let url = site.write("index.html", &html);

  harness.send(support::navigate_msg(
    tab_id,
    url,
    NavigationReason::TypedUrl,
  ));

  // Ensure the document has been rendered at least once so `doc.prepared()` is available for the
  // find index builder.
  let _ = harness.wait_for_frame(tab_id, support::DEFAULT_TIMEOUT);
  let _ = harness.drain_default();

  harness.send(UiToWorker::FindQuery {
    tab_id,
    query: "a".to_string(),
    case_sensitive: false,
  });

  let events = harness.wait_for_event(support::DEFAULT_TIMEOUT, |ev| {
    matches!(
      ev,
      WorkerToUiEvent::FindResult {
        tab_id: msg_tab,
        query,
        ..
      } if *msg_tab == tab_id && query == "a"
    )
  });

  let (match_count, active_match_index) = events
    .iter()
    .rev()
    .find_map(|ev| {
      if let WorkerToUiEvent::FindResult {
        tab_id: msg_tab,
        query,
        match_count,
        active_match_index,
        ..
      } = ev
      {
        (*msg_tab == tab_id && query == "a").then_some((*match_count, *active_match_index))
      } else {
        None
      }
    })
    .expect("FindResult event must be present");

  assert!(
    match_count <= MAX_FIND_MATCHES,
    "find match_count must be capped at {MAX_FIND_MATCHES}, got {match_count}"
  );
  assert_eq!(
    match_count, MAX_FIND_MATCHES,
    "document contains >{MAX_FIND_MATCHES} matches, so find should report the cap"
  );
  assert_eq!(
    active_match_index,
    Some(0),
    "first match should become active when results are non-empty"
  );

  // Ensure the worker keeps producing frames after the find query (no deadlock / runaway work).
  let _ = harness.wait_for_frame(tab_id, Duration::from_secs(10));
  harness.send(support::request_repaint(tab_id, RepaintReason::Explicit));
  let _ = harness.wait_for_frame(tab_id, Duration::from_secs(10));
}

