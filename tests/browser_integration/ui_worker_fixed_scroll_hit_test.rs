#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{
  CursorKind, NavigationReason, PointerButton, RenderedFrame, TabId, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};
use url::Url;

use super::support::{
  create_tab_msg, drain_for, navigate_msg, pointer_down, pointer_move, pointer_up, rgba_at,
  scroll_msg, viewport_changed_msg, TempSite,
};

// Rendering + worker startup can take a few seconds under load when tests run in parallel.
const TIMEOUT: Duration = Duration::from_secs(15);

fn wait_for_frame_ready(
  rx: &impl super::support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
  timeout: Duration,
) -> RenderedFrame {
  let deadline = Instant::now() + timeout;
  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    assert!(
      !remaining.is_zero(),
      "timed out waiting for FrameReady for {tab_id:?}"
    );
    match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(WorkerToUi::FrameReady {
        tab_id: msg_tab,
        frame,
      }) if msg_tab == tab_id => return frame,
      Ok(_) => continue,
      Err(RecvTimeoutError::Timeout) => continue,
      Err(RecvTimeoutError::Disconnected) => panic!("worker disconnected while waiting for frame"),
    }
  }
}

fn wait_for_frame_ready_scrolled_to(
  rx: &impl super::support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
  expected_scroll_y: f32,
  timeout: Duration,
) -> RenderedFrame {
  let deadline = Instant::now() + timeout;
  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    assert!(
      !remaining.is_zero(),
      "timed out waiting for FrameReady with scroll y≈{expected_scroll_y} for {tab_id:?}"
    );
    match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(WorkerToUi::FrameReady {
        tab_id: msg_tab,
        frame,
      }) if msg_tab == tab_id => {
        if (frame.scroll_state.viewport.y - expected_scroll_y).abs() < 2.0 {
          return frame;
        }
      }
      Ok(_) => continue,
      Err(RecvTimeoutError::Timeout) => continue,
      Err(RecvTimeoutError::Disconnected) => {
        panic!("worker disconnected while waiting for scrolled frame")
      }
    }
  }
}

fn wait_for_hover_changed(
  rx: &impl super::support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
  timeout: Duration,
) -> (Option<String>, CursorKind) {
  let deadline = Instant::now() + timeout;
  while Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(WorkerToUi::HoverChanged {
        tab_id: msg_tab,
        hovered_url,
        cursor,
      }) if msg_tab == tab_id => return (hovered_url, cursor),
      Ok(_) => continue,
      Err(RecvTimeoutError::Timeout) => continue,
      Err(RecvTimeoutError::Disconnected) => break,
    }
  }
  panic!("timed out waiting for HoverChanged for {tab_id:?}");
}

fn wait_for_navigation_committed(
  rx: &impl super::support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
  expected_url: &str,
  timeout: Duration,
) {
  let deadline = Instant::now() + timeout;
  let mut started = false;
  let mut committed = false;

  while Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(WorkerToUi::NavigationStarted {
        tab_id: msg_tab,
        url,
      }) if msg_tab == tab_id => {
        if url == expected_url {
          started = true;
        }
      }
      Ok(WorkerToUi::NavigationCommitted {
        tab_id: msg_tab,
        url,
        ..
      }) if msg_tab == tab_id => {
        if url == expected_url {
          committed = true;
          break;
        }
      }
      Ok(WorkerToUi::NavigationFailed {
        tab_id: msg_tab,
        url,
        error,
        ..
      }) if msg_tab == tab_id => {
        panic!("navigation failed for {url}: {error}");
      }
      Ok(_) => {}
      Err(RecvTimeoutError::Timeout) => {}
      Err(RecvTimeoutError::Disconnected) => break,
    }
  }

  assert!(started, "expected NavigationStarted for {expected_url}");
  assert!(committed, "expected NavigationCommitted for {expected_url}");
}

#[test]
fn click_fixed_link_after_scroll_hits_link() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = TempSite::new();
  let page1_url = site.write(
    "page1.html",
    r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin:0; padding:0; }
      a#fixed_link { position:fixed; top:0; left:0; width:200px; height:40px; display:block; background:rgb(255,0,0); }
      #spacer { height: 2000px; }
    </style>
  </head>
  <body>
    <a id="fixed_link" href="page2.html">Go</a>
    <div id="spacer"></div>
  </body>
</html>
"#,
  );
  site.write(
    "page2.html",
    "<!doctype html><html><body>page2</body></html>\n",
  );
  let expected_page2_url = Url::parse(&page1_url)
    .expect("parse page1 url")
    .join("page2.html")
    .expect("resolve page2 url")
    .to_string();

  let handle = spawn_ui_worker("fastr-ui-worker-fixed-scroll-hit-test").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, page1_url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let frame_before_scroll = wait_for_frame_ready(&ui_rx, tab_id, TIMEOUT);
  let px_before_scroll = rgba_at(&frame_before_scroll.pixmap, 150, 20);
  assert!(
    px_before_scroll[0] > 200
      && px_before_scroll[1] < 40
      && px_before_scroll[2] < 40
      && px_before_scroll[3] > 200,
    "expected pixel to be red before scroll, got rgba={px_before_scroll:?}"
  );

  // Older worker versions could emit an initial `ScrollStateUpdated` after navigation, but the
  // protocol no longer guarantees this. Drain briefly so later waits observe the scroll state
  // produced by the scroll message below (without hanging if no such update is sent).
  let _ = drain_for(&ui_rx, Duration::from_millis(200));
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 500.0), None))
    .expect("Scroll");

  let frame_after_scroll = wait_for_frame_ready_scrolled_to(&ui_rx, tab_id, 500.0, TIMEOUT);
  assert!(
    (frame_after_scroll.scroll_state.viewport.y - 500.0).abs() < 2.0,
    "expected viewport scroll y≈500 after scroll, got {}",
    frame_after_scroll.scroll_state.viewport.y
  );

  // Fixed link should stay pinned to the top of the viewport after scrolling.
  let px_after_scroll = rgba_at(&frame_after_scroll.pixmap, 150, 20);
  assert!(
    px_after_scroll[0] > 200
      && px_after_scroll[1] < 40
      && px_after_scroll[2] < 40
      && px_after_scroll[3] > 200,
    "expected pixel to be red after scroll (fixed header), got rgba={px_after_scroll:?}"
  );

  // Hover should resolve the fixed link after scrolling.
  ui_tx
    .send(pointer_move(tab_id, (10.0, 10.0), PointerButton::None))
    .expect("PointerMove");
  let (hovered_url, cursor) = wait_for_hover_changed(&ui_rx, tab_id, TIMEOUT);
  assert_eq!(cursor, CursorKind::Pointer);
  assert_eq!(hovered_url.as_deref(), Some(expected_page2_url.as_str()));

  ui_tx
    .send(pointer_down(tab_id, (10.0, 10.0), PointerButton::Primary))
    .expect("PointerDown");
  ui_tx
    .send(pointer_up(tab_id, (10.0, 10.0), PointerButton::Primary))
    .expect("PointerUp");

  wait_for_navigation_committed(&ui_rx, tab_id, &expected_page2_url, TIMEOUT);

  drop(ui_tx);
  join.join().expect("join ui worker");
}
