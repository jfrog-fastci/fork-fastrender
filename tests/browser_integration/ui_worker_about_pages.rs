#![cfg(feature = "browser_ui")]

use super::support::{create_tab_msg, navigate_msg, viewport_changed_msg, DEFAULT_TIMEOUT};
use fastrender::ui::about_pages;
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

// Worker startup + the first navigation can take a few seconds under load when integration tests
// run in parallel on CI.
const TIMEOUT: Duration = DEFAULT_TIMEOUT;

fn wait_for_navigation_committed_and_frame(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  expected_url: &str,
  expected_title: &str,
  timeout: Duration,
) -> fastrender::ui::messages::RenderedFrame {
  let deadline = Instant::now() + timeout;
  let mut committed = false;

  loop {
    let now = Instant::now();
    if now >= deadline {
      panic!("timed out waiting for NavigationCommitted+FrameReady for {expected_url}");
    }
    let remaining = deadline - now;
    match rx.recv_timeout(remaining) {
      Ok(msg) => match msg {
        WorkerToUi::NavigationCommitted {
          tab_id: msg_tab,
          url,
          title,
          ..
        } if msg_tab == tab_id && url == expected_url => {
          assert_eq!(
            title,
            Some(expected_title.to_string()),
            "unexpected title for {expected_url}"
          );
          committed = true;
        }
        WorkerToUi::NavigationFailed {
          tab_id: msg_tab,
          url,
          error,
          ..
        } if msg_tab == tab_id && url == expected_url => {
          panic!("navigation failed for {url}: {error}");
        }
        WorkerToUi::FrameReady {
          tab_id: msg_tab,
          frame,
        } if committed && msg_tab == tab_id => return frame,
        _ => {}
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
        panic!("worker channel disconnected while waiting for {expected_url}");
      }
    }
  }
}

#[test]
fn about_pages_render_and_have_titles() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let handle = spawn_ui_worker("ui_worker_about_pages").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  // Basic HTML smoke test for the new tab page. (The full rendering validation happens below via
  // UI worker navigation.)
  let newtab_html =
    about_pages::html_for_about_url(about_pages::ABOUT_NEWTAB).expect("about:newtab HTML");
  for url in [
    "https://example.com/",
    about_pages::ABOUT_HELP,
    about_pages::ABOUT_VERSION,
    about_pages::ABOUT_GPU,
  ] {
    assert!(
      newtab_html.contains(url),
      "expected about:newtab HTML to contain a link to {url}"
    );
  }

  let layout_stress_html = about_pages::html_for_about_url(about_pages::ABOUT_TEST_LAYOUT_STRESS)
    .expect("about:test-layout-stress HTML");
  assert!(
    !layout_stress_html.trim().is_empty(),
    "expected about:test-layout-stress HTML to be non-empty"
  );

  let tab = TabId::new();
  ui_tx.send(create_tab_msg(tab, None)).expect("create tab");

  for (url, title) in [
    ("about:newtab", "New Tab"),
    ("about:history", "History"),
    ("about:bookmarks", "Bookmarks"),
    ("about:help", "Help"),
    ("about:version", "Version"),
    ("about:gpu", "GPU"),
    ("about:test-layout-stress", "Layout Stress Test"),
  ] {
    ui_tx
      .send(navigate_msg(
        tab,
        url.to_string(),
        NavigationReason::TypedUrl,
      ))
      .unwrap_or_else(|_| panic!("navigate to {url}"));
    let _frame = wait_for_navigation_committed_and_frame(&ui_rx, tab, url, title, TIMEOUT);
  }

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

#[test]
fn layout_stress_page_reflows_with_viewport_width() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let handle = spawn_ui_worker("ui_worker_about_layout_stress_reflow").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab = TabId::new();
  ui_tx.send(create_tab_msg(tab, None)).expect("create tab");

  // Choose widths that force a different number of grid columns for the `minmax(240px, 1fr)` cards.
  let wide_viewport = (900, 600);
  let narrow_viewport = (360, 600);
  ui_tx
    .send(viewport_changed_msg(tab, wide_viewport, 1.0))
    .expect("wide viewport");

  let url = about_pages::ABOUT_TEST_LAYOUT_STRESS.to_string();
  ui_tx
    .send(navigate_msg(tab, url.clone(), NavigationReason::TypedUrl))
    .expect("navigate");

  let wide_frame =
    wait_for_navigation_committed_and_frame(&ui_rx, tab, &url, "Layout Stress Test", TIMEOUT);
  assert_eq!(
    wide_frame.viewport_css, wide_viewport,
    "expected first frame after navigating to layout-stress to use the configured wide viewport"
  );
  let wide_height = wide_frame.scroll_metrics.content_css.1;

  ui_tx
    .send(viewport_changed_msg(tab, narrow_viewport, 1.0))
    .expect("narrow viewport");

  let narrow_frame_msg = super::support::recv_for_tab(&ui_rx, tab, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { frame, .. } if frame.viewport_css == narrow_viewport)
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after narrowing viewport for {url}"));

  let WorkerToUi::FrameReady {
    frame: narrow_frame,
    ..
  } = narrow_frame_msg
  else {
    unreachable!();
  };
  let narrow_height = narrow_frame.scroll_metrics.content_css.1;

  assert!(
    narrow_height > wide_height * 1.25,
    "expected layout-stress content height to increase meaningfully when narrowing viewport: wide={wide_height} narrow={narrow_height}"
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
