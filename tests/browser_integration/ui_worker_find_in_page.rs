#![cfg(feature = "browser_ui")]

use super::support;
use super::worker_harness::{WorkerHarness, WorkerToUiEvent};
use fastrender::ui::messages::{NavigationReason, RenderedFrame, TabId, UiToWorker};

fn find_fixture() -> (support::TempSite, String) {
  let site = support::TempSite::new();
  // Deterministic fixture:
  // - four occurrences of the query, one with different case
  // - occurrences separated across multiple lines and far enough apart that some are off-screen
  // - transparent text on white background so highlight overlays are easy to detect by pixel probes.
  let html = r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body {
        margin: 0;
        padding: 0;
        background: rgb(255, 255, 255);
      }
      body {
        font-size: 64px;
        line-height: 80px;
        font-family: monospace;
        color: rgba(0, 0, 0, 0);
      }
      .spacer { height: 1200px; }
    </style>
  </head>
  <body>
    <div id="m0">needle</div>
    <div id="m1">Needle</div>
    <div class="spacer"></div>
    <div id="m2">needle</div>
    <div class="spacer"></div>
    <div id="m3">needle</div>
    <div class="spacer"></div>
  </body>
</html>
"#;
  let url = site.write("index.html", html);
  (site, url)
}

fn rgba_at_css(frame: &RenderedFrame, x_css: u32, y_css: u32) -> [u8; 4] {
  let x_px = ((x_css as f32) * frame.dpr).round() as u32;
  let y_px = ((y_css as f32) * frame.dpr).round() as u32;
  support::rgba_at(&frame.pixmap, x_px, y_px)
}

fn last_find_result(events: &[WorkerToUiEvent]) -> (String, bool, usize, Option<usize>) {
  for ev in events.iter().rev() {
    if let WorkerToUiEvent::FindResult {
      query,
      case_sensitive,
      match_count,
      active_match_index,
      ..
    } = ev
    {
      return (
        query.clone(),
        *case_sensitive,
        *match_count,
        *active_match_index,
      );
    }
  }
  panic!("expected WorkerToUi::FindResult in events, got {events:?}");
}

#[test]
fn ui_worker_find_in_page_results_and_scroll() {
  let _lock = super::stage_listener_test_lock();
  let (_site, url) = find_fixture();

  let h = WorkerHarness::spawn();
  let tab_id = TabId::new();

  let viewport = (320, 200);
  let dpr = 1.0;

  h.send(support::create_tab_msg(tab_id, None));
  h.send(support::viewport_changed_msg(tab_id, viewport, dpr));
  h.send(support::navigate_msg(
    tab_id,
    url,
    NavigationReason::TypedUrl,
  ));

  let (frame0, _events) = h.wait_for_frame(tab_id, support::DEFAULT_TIMEOUT);
  assert!(
    frame0.scroll_state.viewport.y.abs() < 1e-3,
    "expected initial scroll_y=0, got {:?}",
    frame0.scroll_state.viewport
  );

  // ---------------------------------------------------------------------------
  // Query with 0 matches: should report active_match_index=None and not scroll.
  // ---------------------------------------------------------------------------
  let (frame1, events1) = h.send_and_wait_for_frame(
    tab_id,
    UiToWorker::FindQuery {
      tab_id,
      query: "does-not-exist".to_string(),
      case_sensitive: false,
    },
  );
  let (query, case_sensitive, match_count, active) = last_find_result(&events1);
  assert_eq!(query, "does-not-exist");
  assert!(!case_sensitive);
  assert_eq!(match_count, 0);
  assert_eq!(
    active, None,
    "expected active_match_index=None when match_count=0"
  );
  assert!(
    frame1.scroll_state.viewport.y.abs() < 1e-3,
    "expected FindQuery(0 matches) not to scroll, got {:?}",
    frame1.scroll_state.viewport
  );

  // ---------------------------------------------------------------------------
  // Case-sensitive vs case-insensitive match counting.
  // ---------------------------------------------------------------------------
  let (frame2, events2) = h.send_and_wait_for_frame(
    tab_id,
    UiToWorker::FindQuery {
      tab_id,
      query: "needle".to_string(),
      case_sensitive: true,
    },
  );
  let (query, case_sensitive, match_count, active) = last_find_result(&events2);
  assert_eq!(query, "needle");
  assert!(case_sensitive);
  assert_eq!(
    match_count, 3,
    "expected only lowercase occurrences to match case-sensitive query"
  );
  assert_eq!(
    active,
    Some(0),
    "expected active_match_index to be 0-based when match_count>0"
  );
  assert!(
    frame2.scroll_state.viewport.y.abs() < 1e-3,
    "expected initial FindQuery to keep first match in view without scrolling, got {:?}",
    frame2.scroll_state.viewport
  );

  let (frame3, events3) = h.send_and_wait_for_frame(
    tab_id,
    UiToWorker::FindQuery {
      tab_id,
      query: "needle".to_string(),
      case_sensitive: false,
    },
  );
  let (query, case_sensitive, match_count, active) = last_find_result(&events3);
  assert_eq!(query, "needle");
  assert!(!case_sensitive);
  assert_eq!(
    match_count, 4,
    "expected case-insensitive query to match all occurrences"
  );
  assert_eq!(active, Some(0));
  assert!(
    frame3.scroll_state.viewport.y.abs() < 1e-3,
    "expected FindQuery to activate match 0 without scrolling when already visible, got {:?}",
    frame3.scroll_state.viewport
  );

  // ---------------------------------------------------------------------------
  // FindNext scrolls to off-screen matches and wraps around.
  // ---------------------------------------------------------------------------
  let (frame4, events4) = h.send_and_wait_for_frame(tab_id, UiToWorker::FindNext { tab_id });
  let (_query, _case_sensitive, match_count, active) = last_find_result(&events4);
  assert_eq!(match_count, 4);
  assert_eq!(active, Some(1));
  assert!(
    (frame4.scroll_state.viewport.y - frame3.scroll_state.viewport.y).abs() < 1e-3,
    "expected FindNext to second on-screen match not to scroll (before={}, after={})",
    frame3.scroll_state.viewport.y,
    frame4.scroll_state.viewport.y
  );

  let (frame5, events5) = h.send_and_wait_for_frame(tab_id, UiToWorker::FindNext { tab_id });
  let (_query, _case_sensitive, match_count, active) = last_find_result(&events5);
  assert_eq!(match_count, 4);
  assert_eq!(active, Some(2));
  assert!(
    frame5.scroll_state.viewport.y > frame4.scroll_state.viewport.y + 100.0,
    "expected FindNext to off-screen match to scroll down (before={}, after={})",
    frame4.scroll_state.viewport.y,
    frame5.scroll_state.viewport.y
  );

  let (frame6, events6) = h.send_and_wait_for_frame(tab_id, UiToWorker::FindNext { tab_id });
  let (_query, _case_sensitive, match_count, active) = last_find_result(&events6);
  assert_eq!(match_count, 4);
  assert_eq!(active, Some(3));
  assert!(
    frame6.scroll_state.viewport.y > frame5.scroll_state.viewport.y + 100.0,
    "expected FindNext to later off-screen match to scroll further down (before={}, after={})",
    frame5.scroll_state.viewport.y,
    frame6.scroll_state.viewport.y
  );

  let (frame7, events7) = h.send_and_wait_for_frame(tab_id, UiToWorker::FindNext { tab_id });
  let (_query, _case_sensitive, match_count, active) = last_find_result(&events7);
  assert_eq!(match_count, 4);
  assert_eq!(
    active,
    Some(0),
    "expected FindNext to wrap around to match 0"
  );
  assert!(
    frame7.scroll_state.viewport.y + 10.0 < frame6.scroll_state.viewport.y,
    "expected wrap-around FindNext to scroll back up (before={}, after={})",
    frame6.scroll_state.viewport.y,
    frame7.scroll_state.viewport.y
  );

  // ---------------------------------------------------------------------------
  // FindPrev wraps around and scrolls upward/downward accordingly.
  // ---------------------------------------------------------------------------
  let (frame8, events8) = h.send_and_wait_for_frame(tab_id, UiToWorker::FindPrev { tab_id });
  let (_query, _case_sensitive, match_count, active) = last_find_result(&events8);
  assert_eq!(match_count, 4);
  assert_eq!(
    active,
    Some(3),
    "expected FindPrev to wrap around to last match"
  );
  assert!(
    frame8.scroll_state.viewport.y > frame7.scroll_state.viewport.y + 100.0,
    "expected wrap-around FindPrev to scroll down to last match (before={}, after={})",
    frame7.scroll_state.viewport.y,
    frame8.scroll_state.viewport.y
  );
}

#[test]
fn ui_worker_find_in_page_highlight_and_clear() {
  let _lock = super::stage_listener_test_lock();
  let (_site, url) = find_fixture();

  let h = WorkerHarness::spawn();
  let tab_id = TabId::new();

  let viewport = (320, 200);
  let dpr = 1.0;

  h.send(support::create_tab_msg(tab_id, None));
  h.send(support::viewport_changed_msg(tab_id, viewport, dpr));
  h.send(support::navigate_msg(
    tab_id,
    url,
    NavigationReason::TypedUrl,
  ));

  let (baseline_frame, _events) = h.wait_for_frame(tab_id, support::DEFAULT_TIMEOUT);
  let m0_css = (20, 40);
  let m1_css = (20, 120);

  let baseline0 = rgba_at_css(&baseline_frame, m0_css.0, m0_css.1);
  let baseline1 = rgba_at_css(&baseline_frame, m1_css.0, m1_css.1);
  assert_eq!(
    baseline0,
    [255, 255, 255, 255],
    "expected baseline to be white"
  );
  assert_eq!(
    baseline1,
    [255, 255, 255, 255],
    "expected baseline to be white"
  );

  // Start find-in-page. Match0 is active; match1 is inactive.
  let (frame1, events1) = h.send_and_wait_for_frame(
    tab_id,
    UiToWorker::FindQuery {
      tab_id,
      query: "needle".to_string(),
      case_sensitive: false,
    },
  );
  let (_query, _case_sensitive, match_count, active) = last_find_result(&events1);
  assert_eq!(match_count, 4);
  assert_eq!(active, Some(0));

  let pix_active0 = rgba_at_css(&frame1, m0_css.0, m0_css.1);
  let pix_inactive1 = rgba_at_css(&frame1, m1_css.0, m1_css.1);
  assert_ne!(
    pix_active0, baseline0,
    "expected active match to be highlighted (pixel changed from baseline)"
  );
  assert_ne!(
    pix_inactive1, baseline1,
    "expected inactive match to be highlighted (pixel changed from baseline)"
  );
  assert_ne!(
    pix_active0, pix_inactive1,
    "expected active/inactive highlight tints to differ"
  );

  // Advance active match within the on-screen region. Highlights should swap.
  let (frame2, events2) = h.send_and_wait_for_frame(tab_id, UiToWorker::FindNext { tab_id });
  let (_query, _case_sensitive, match_count, active) = last_find_result(&events2);
  assert_eq!(match_count, 4);
  assert_eq!(active, Some(1));

  let pix_inactive0 = rgba_at_css(&frame2, m0_css.0, m0_css.1);
  let pix_active1 = rgba_at_css(&frame2, m1_css.0, m1_css.1);
  assert_eq!(
    pix_inactive0, pix_inactive1,
    "expected inactive highlight tint to move to match 0 after FindNext"
  );
  assert_eq!(
    pix_active1, pix_active0,
    "expected active highlight tint to move to match 1 after FindNext"
  );

  // FindStop clears highlights.
  let (frame3, events3) = h.send_and_wait_for_frame(tab_id, UiToWorker::FindStop { tab_id });
  let (query, case_sensitive, match_count, active) = last_find_result(&events3);
  assert_eq!(query, "");
  assert!(!case_sensitive);
  assert_eq!(match_count, 0);
  assert_eq!(active, None);

  assert_eq!(rgba_at_css(&frame3, m0_css.0, m0_css.1), baseline0);
  assert_eq!(rgba_at_css(&frame3, m1_css.0, m1_css.1), baseline1);

  // Restart find and clear via FindQuery { query: "" }.
  let (frame4, events4) = h.send_and_wait_for_frame(
    tab_id,
    UiToWorker::FindQuery {
      tab_id,
      query: "needle".to_string(),
      case_sensitive: false,
    },
  );
  let (_query, _case_sensitive, match_count, active) = last_find_result(&events4);
  assert_eq!(match_count, 4);
  assert_eq!(active, Some(0));
  assert_eq!(
    rgba_at_css(&frame4, m0_css.0, m0_css.1),
    pix_active0,
    "expected highlight tint to reappear after restarting FindQuery"
  );

  let (frame5, events5) = h.send_and_wait_for_frame(
    tab_id,
    UiToWorker::FindQuery {
      tab_id,
      query: "".to_string(),
      case_sensitive: false,
    },
  );
  let (query, case_sensitive, match_count, active) = last_find_result(&events5);
  assert_eq!(query, "");
  assert!(!case_sensitive);
  assert_eq!(match_count, 0);
  assert_eq!(active, None);

  assert_eq!(rgba_at_css(&frame5, m0_css.0, m0_css.1), baseline0);
  assert_eq!(rgba_at_css(&frame5, m1_css.0, m1_css.1), baseline1);
}
