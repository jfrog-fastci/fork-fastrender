#![cfg(feature = "browser_ui")]

use super::support;
use super::worker_harness::{WorkerHarness, WorkerToUiEvent};
use fastrender::ui::messages::{NavigationReason, RepaintReason, TabId, UiToWorker};
use fastrender::ui::render_worker::{
  last_scroll_blit_fallback_reason_for_test, reset_scroll_blit_fallback_reason_for_test,
};

fn find_fixture() -> (support::TempSite, String) {
  let site = support::TempSite::new();
  // Build a long page with repeated matches so find highlights cover the viewport at both scroll
  // positions. Make the text fully transparent so the highlight overlay is the only visible ink.
  let mut lines = String::new();
  for i in 0..200 {
    lines.push_str(&format!("<div>needle needle needle line {i}</div>\n"));
  }
  let html = format!(
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body {{
        margin: 0;
        padding: 0;
        background: rgb(255, 255, 255);
      }}
      body {{
        font-size: 64px;
        line-height: 80px;
        font-family: monospace;
        /* Make text invisible so highlight alpha differences are easy to detect. */
        color: rgba(0, 0, 0, 0);
      }}
    </style>
  </head>
  <body>
    {lines}
  </body>
</html>"#
  );
  let url = site.write("index.html", &html);
  (site, url)
}

fn last_find_match_count(events: &[WorkerToUiEvent]) -> Option<usize> {
  events.iter().rev().find_map(|ev| match ev {
    WorkerToUiEvent::FindResult { match_count, .. } => Some(*match_count),
    _ => None,
  })
}

#[test]
fn ui_worker_find_highlight_scroll_frame_matches_full_repaint() {
  let _lock = super::stage_listener_test_lock();
  support::ensure_bundled_fonts_loaded();
  reset_scroll_blit_fallback_reason_for_test();

  let (_site, url) = find_fixture();
  let h = WorkerHarness::spawn();
  let tab_id = TabId::new();

  let viewport = (320, 200);
  let dpr = 1.0;
  let scroll_delta_y = 20.0;

  h.send(support::create_tab_msg(tab_id, None));
  h.send(support::viewport_changed_msg(tab_id, viewport, dpr));
  h.send(support::navigate_msg(
    tab_id,
    url,
    NavigationReason::TypedUrl,
  ));

  let (_frame0, _events0) = h.wait_for_frame(tab_id, support::DEFAULT_TIMEOUT);

  // Enable find-in-page highlighting.
  let (_frame1, events1) = h.send_and_wait_for_frame(
    tab_id,
    UiToWorker::FindQuery {
      tab_id,
      query: "needle".to_string(),
      case_sensitive: false,
    },
  );
  let match_count = last_find_match_count(&events1).expect("expected a FindResult after FindQuery");
  assert!(match_count > 0, "expected find query to match at least once");

  // Reset the fallback reason so any value we observe comes from the scroll repaint.
  reset_scroll_blit_fallback_reason_for_test();

  // Scroll by an integer device-pixel delta so scroll blit would otherwise be eligible.
  let (scrolled_frame, _events2) =
    h.send_and_wait_for_frame(tab_id, support::scroll_msg(tab_id, (0.0, scroll_delta_y), None));
  assert!(
    scrolled_frame.scroll_state.viewport.y > 0.0,
    "expected scroll to move viewport.y > 0, got {:?}",
    scrolled_frame.scroll_state.viewport
  );

  assert_eq!(
    last_scroll_blit_fallback_reason_for_test(),
    Some("FindHighlightActive"),
    "expected scroll blit to be disabled when find highlighting is active"
  );

  // Force a full repaint at the same scroll position and compare bytes. If scroll blit were used
  // without incremental highlight reapplication, the scrolled frame would differ from this
  // reference (double-applied alpha in the overlapping region).
  let (full_repaint, _events3) = h.send_and_wait_for_frame(
    tab_id,
    support::request_repaint(tab_id, RepaintReason::Explicit),
  );
  assert!(
    (full_repaint.scroll_state.viewport.y - scrolled_frame.scroll_state.viewport.y).abs() < 1e-3,
    "expected forced repaint to keep scroll_y constant (before={}, after={})",
    scrolled_frame.scroll_state.viewport.y,
    full_repaint.scroll_state.viewport.y
  );

  assert_eq!(
    scrolled_frame.pixmap.data(),
    full_repaint.pixmap.data(),
    "scroll repaint with find highlights should match a forced full repaint"
  );
}

