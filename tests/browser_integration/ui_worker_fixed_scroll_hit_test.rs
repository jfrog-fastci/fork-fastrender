#![cfg(feature = "browser_ui")]

use fastrender::scroll::ScrollState;
use fastrender::ui::messages::{
  CursorKind, NavigationReason, PointerButton, RenderedFrame, TabId, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use url::Url;

use super::support::{
  create_tab_msg, navigate_msg, pointer_down, pointer_move, pointer_up, rgba_at, scroll_msg,
  viewport_changed_msg, TempSite,
};

// Rendering + worker startup can take a few seconds under load when tests run in parallel.
const TIMEOUT: Duration = Duration::from_secs(15);

fn wait_for_frame_ready(
  rx: &Receiver<WorkerToUi>,
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

fn wait_for_scroll_and_frame(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  timeout: Duration,
) -> (ScrollState, RenderedFrame) {
  let deadline = Instant::now() + timeout;
  let mut scroll: Option<ScrollState> = None;
  let mut frame: Option<RenderedFrame> = None;

  while Instant::now() < deadline {
    if scroll.is_some() && frame.is_some() {
      break;
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(WorkerToUi::ScrollStateUpdated {
        tab_id: msg_tab,
        scroll: s,
      }) if msg_tab == tab_id => {
        scroll = Some(s);
      }
      Ok(WorkerToUi::FrameReady {
        tab_id: msg_tab,
        frame: f,
      }) if msg_tab == tab_id => {
        frame = Some(f);
      }
      Ok(_) => {}
      Err(RecvTimeoutError::Timeout) => {}
      Err(RecvTimeoutError::Disconnected) => break,
    }
  }

  (
    scroll.expect("expected ScrollStateUpdated after scroll"),
    frame.expect("expected FrameReady after scroll"),
  )
}

fn wait_for_scroll_state_updated(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  timeout: Duration,
) -> ScrollState {
  let deadline = Instant::now() + timeout;
  while Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(WorkerToUi::ScrollStateUpdated {
        tab_id: msg_tab,
        scroll,
      }) if msg_tab == tab_id => {
        return scroll;
      }
      Ok(_) => continue,
      Err(RecvTimeoutError::Timeout) => continue,
      Err(RecvTimeoutError::Disconnected) => break,
    }
  }
  panic!("timed out waiting for ScrollStateUpdated for {tab_id:?}");
}

fn wait_for_hover_changed(
  rx: &Receiver<WorkerToUi>,
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
  rx: &Receiver<WorkerToUi>,
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

  // Drain the initial scroll state update emitted by the worker so later assertions observe the
  // scroll state produced by the scroll message below.
  let _ = wait_for_scroll_state_updated(&ui_rx, tab_id, TIMEOUT);

  ui_tx
    .send(scroll_msg(tab_id, (0.0, 500.0), None))
    .expect("Scroll");

  let (scroll, frame_after_scroll) = wait_for_scroll_and_frame(&ui_rx, tab_id, TIMEOUT);
  assert!(
    (scroll.viewport.y - 500.0).abs() < 2.0,
    "expected viewport scroll y≈500 after scroll, got {}",
    scroll.viewport.y
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
