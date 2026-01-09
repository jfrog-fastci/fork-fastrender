#![cfg(feature = "browser_ui")]

use fastrender::scroll::ScrollState;
use fastrender::ui::messages::{
  NavigationReason, PointerButton, RenderedFrame, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::worker_loop::spawn_ui_worker;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use url::Url;

use super::support::{
  create_tab_msg, navigate_msg, rgba_at, scroll_msg, viewport_changed_msg, TempSite,
};

// Rendering + worker startup can take a few seconds under load when tests run in parallel.
const TIMEOUT: Duration = Duration::from_secs(15);

fn wait_for_frame_ready(rx: &Receiver<WorkerToUi>, tab_id: TabId, timeout: Duration) -> RenderedFrame {
  let deadline = Instant::now() + timeout;
  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    assert!(
      !remaining.is_zero(),
      "timed out waiting for FrameReady for {tab_id:?}"
    );
    match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(WorkerToUi::FrameReady { tab_id: msg_tab, frame }) if msg_tab == tab_id => return frame,
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
      Ok(WorkerToUi::ScrollStateUpdated { tab_id: msg_tab, scroll: s }) if msg_tab == tab_id => {
        scroll = Some(s);
      }
      Ok(WorkerToUi::FrameReady { tab_id: msg_tab, frame: f }) if msg_tab == tab_id => {
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
      Ok(WorkerToUi::NavigationStarted { tab_id: msg_tab, url }) if msg_tab == tab_id => {
        if url == expected_url {
          started = true;
        }
      }
      Ok(WorkerToUi::NavigationCommitted { tab_id: msg_tab, url, .. }) if msg_tab == tab_id => {
        if url == expected_url {
          committed = true;
          break;
        }
      }
      Ok(WorkerToUi::NavigationFailed { tab_id: msg_tab, url, error, .. }) if msg_tab == tab_id => {
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
fn click_after_scroll_hits_link() {
  let _lock = super::stage_listener_test_lock();
  let site = TempSite::new();
  let page1_url = site.write(
    "page1.html",
    r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin:0; padding:0; }
      a#link { position:absolute; top:500px; left:0; width:200px; height:40px; display:block; background:rgb(255,0,0); }
    </style>
  </head>
  <body>
    <a id="link" href="page2.html">Go</a>
  </body>
</html>
"#,
  );
  site.write("page2.html", "<!doctype html><html><body>page2</body></html>\n");
  let expected_page2_url = Url::parse(&page1_url)
    .expect("parse page1 url")
    .join("page2.html")
    .expect("resolve page2 url")
    .to_string();

  let handle =
    spawn_ui_worker("fastr-ui-worker-scroll-hit-test").expect("spawn ui worker scroll hit test");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("CreateTab");
  // Keep the viewport height <= the link height so scrolling to y=500 is possible even when the
  // document's scrollable height is only just large enough to include the absolute-positioned link.
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 40), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, page1_url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let frame_before_scroll = wait_for_frame_ready(&ui_rx, tab_id, TIMEOUT);
  let px_before_scroll = rgba_at(&frame_before_scroll.pixmap, 150, 20);
  assert!(
    !(px_before_scroll[0] > 200
      && px_before_scroll[1] < 40
      && px_before_scroll[2] < 40
      && px_before_scroll[3] > 200),
    "expected pixel to not be red before scroll, got rgba={px_before_scroll:?}"
  );

  ui_tx
    .send(scroll_msg(tab_id, (0.0, 500.0), None))
    .expect("Scroll");

  let (scroll, frame_after_scroll) = wait_for_scroll_and_frame(&ui_rx, tab_id, TIMEOUT);
  assert!(
    (scroll.viewport.y - 500.0).abs() < 2.0,
    "expected viewport scroll y≈500 after scroll, got {}",
    scroll.viewport.y
  );

  // Prove the link is actually in view: the element is a solid red block at the top after scroll.
  let px = rgba_at(&frame_after_scroll.pixmap, 150, 20);
  assert!(
    px[0] > 200 && px[1] < 40 && px[2] < 40 && px[3] > 200,
    "expected pixel to be red after scroll, got rgba={px:?}"
  );

  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .expect("PointerDown");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
    })
    .expect("PointerUp");

  wait_for_navigation_committed(&ui_rx, tab_id, &expected_page2_url, TIMEOUT);

  drop(ui_tx);
  join.join().expect("join ui worker");
}
