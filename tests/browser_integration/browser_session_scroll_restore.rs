#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{RenderedFrame, TabId, UiToWorker, WorkerToUi};
use std::time::{Duration, Instant};

// Worker startup + navigation + render can take a few seconds under parallel load (CI).
const TIMEOUT: Duration = Duration::from_secs(20);

fn recv_frame_matching_scroll(
  rx: &std::sync::mpsc::Receiver<WorkerToUi>,
  tab_id: TabId,
  expected_scroll_css: (f32, f32),
) -> RenderedFrame {
  let deadline = Instant::now() + TIMEOUT;
  while Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let msg = support::recv_for_tab(
      rx,
      tab_id,
      remaining.min(Duration::from_millis(200)),
      |msg| {
        matches!(
          msg,
          WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. }
        )
      },
    );
    let Some(msg) = msg else { continue };
    match msg {
      WorkerToUi::FrameReady { frame, .. } => {
        let dx = (frame.scroll_state.viewport.x - expected_scroll_css.0).abs();
        let dy = (frame.scroll_state.viewport.y - expected_scroll_css.1).abs();
        if dx < 2.0 && dy < 2.0 {
          return frame;
        }
      }
      WorkerToUi::NavigationFailed { url, error, .. } => {
        panic!("navigation failed for {url}: {error}");
      }
      _ => {}
    }
  }
  panic!(
    "timed out waiting for FrameReady with scroll_css ~= {expected_scroll_css:?} for tab {tab_id:?}"
  );
}

#[test]
fn browser_session_restores_scroll_position_via_scroll_to() {
  let _lock = super::stage_listener_test_lock();

  const VIEWPORT_CSS: (u32, u32) = (240, 120);
  const DPR: f32 = 1.0;

  // First run: produce a realistic scroll offset for about:test-scroll.
  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let fastrender::ui::BrowserWorkerHandle { tx, rx, join } = worker;

  let tab_id = TabId::new();
  tx.send(support::create_tab_msg(
    tab_id,
    Some("about:test-scroll".to_string()),
  ))
  .expect("CreateTab");
  tx.send(UiToWorker::SetActiveTab { tab_id })
    .expect("SetActiveTab");
  tx.send(support::viewport_changed_msg(tab_id, VIEWPORT_CSS, DPR))
    .expect("ViewportChanged");

  // Wait for any initial frame so we know the document is ready.
  let msg = support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::FrameReady { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for initial FrameReady for tab {tab_id:?}"));
  if let WorkerToUi::NavigationFailed { url, error, .. } = msg {
    panic!("navigation failed for {url}: {error}");
  }

  let requested_scroll_css = (0.0, 240.0);
  tx.send(support::scroll_to_msg(tab_id, requested_scroll_css))
    .expect("ScrollTo");
  tx.send(support::request_repaint(
    tab_id,
    fastrender::ui::RepaintReason::Scroll,
  ))
  .expect("RequestRepaint");

  let scrolled_frame = recv_frame_matching_scroll(&rx, tab_id, requested_scroll_css);
  let saved_scroll_css = (
    scrolled_frame.scroll_state.viewport.x,
    scrolled_frame.scroll_state.viewport.y,
  );
  assert!(
    saved_scroll_css.1 > 0.0,
    "expected scroll_y to be non-zero after scrolling, got {saved_scroll_css:?}"
  );

  drop(tx);
  drop(rx);
  join.join().expect("worker join");

  // Simulate persistence.
  let session = fastrender::ui::BrowserSession {
    version: 2,
    home_url: fastrender::ui::about_pages::ABOUT_NEWTAB.to_string(),
    windows: vec![fastrender::ui::BrowserSessionWindow {
      tabs: vec![fastrender::ui::BrowserSessionTab {
        url: "about:test-scroll".to_string(),
        zoom: None,
        scroll_css: Some(saved_scroll_css),
        pinned: false,
        group: None,
      }],
      tab_groups: Vec::new(),
      active_tab_index: 0,
      bookmarks_bar_visible: false,
      show_menu_bar: !cfg!(target_os = "macos"),
      window_state: None,
    }],
    active_window_index: 0,
    appearance: fastrender::ui::appearance::AppearanceSettings::default(),
    did_exit_cleanly: true,
    unclean_exit_streak: 0,
    ui_scale: None,
  }
  .sanitized();

  // Second run: restore the scroll offset after establishing a viewport.
  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let fastrender::ui::BrowserWorkerHandle { tx, rx, join } = worker;

  let restored_tab_id = TabId::new();
  tx.send(support::create_tab_msg(
    restored_tab_id,
    Some(session.windows[0].tabs[0].url.clone()),
  ))
  .expect("CreateTab (restored)");
  tx.send(UiToWorker::SetActiveTab {
    tab_id: restored_tab_id,
  })
  .expect("SetActiveTab (restored)");
  tx.send(support::viewport_changed_msg(
    restored_tab_id,
    VIEWPORT_CSS,
    DPR,
  ))
  .expect("ViewportChanged (restored)");

  if let Some(pos_css) = session.windows[0].tabs[0].scroll_css {
    tx.send(support::scroll_to_msg(restored_tab_id, pos_css))
      .expect("ScrollTo (restored)");
    tx.send(support::request_repaint(
      restored_tab_id,
      fastrender::ui::RepaintReason::Scroll,
    ))
    .expect("RequestRepaint (restored)");
  }

  let restored_frame = recv_frame_matching_scroll(&rx, restored_tab_id, saved_scroll_css);
  assert!(
    (restored_frame.scroll_state.viewport.y - saved_scroll_css.1).abs() < 2.0,
    "expected restored scroll_y ~= {} (got {:?})",
    saved_scroll_css.1,
    restored_frame.scroll_state.viewport
  );

  drop(tx);
  drop(rx);
  join.join().expect("worker join");
}
