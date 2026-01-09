#![cfg(feature = "browser_ui")]

use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::worker::spawn_ui_worker;
use std::ffi::OsString;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

struct EnvVarGuard {
  key: &'static str,
  previous: Option<OsString>,
}

impl EnvVarGuard {
  fn set(key: &'static str, value: &str) -> Self {
    let previous = std::env::var_os(key);
    std::env::set_var(key, value);
    Self { key, previous }
  }
}

impl Drop for EnvVarGuard {
  fn drop(&mut self) {
    match self.previous.take() {
      Some(value) => std::env::set_var(self.key, value),
      None => std::env::remove_var(self.key),
    }
  }
}

fn pixmap_is_uniform_rgba(pixmap: &tiny_skia::Pixmap) -> bool {
  let data = pixmap.data();
  let Some(first) = data.get(0..4) else {
    return true;
  };
  data.chunks_exact(4).all(|px| px == first)
}

fn wait_for_navigation_started(
  rx: &std::sync::mpsc::Receiver<WorkerToUi>,
  tab_id: TabId,
  expected_url: &str,
  timeout: Duration,
) {
  let deadline = Instant::now() + timeout;
  while Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(WorkerToUi::NavigationStarted { tab_id: msg_tab, url }) if msg_tab == tab_id => {
        if url == expected_url {
          return;
        }
      }
      Ok(_) => {}
      Err(RecvTimeoutError::Timeout) => {}
      Err(RecvTimeoutError::Disconnected) => break,
    }
  }
  panic!("timed out waiting for NavigationStarted({expected_url}) for {tab_id:?}");
}

#[test]
fn nav_generation_cancels_in_flight_navigation_and_drops_stale_frame() {
  let handle = spawn_ui_worker("fastr-ui-worker-cancel-nav").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.into_parts();

  let tab_id = TabId::new();
  let cancel = CancelGens::new();

  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: cancel.clone(),
    })
    .expect("CreateTab");
  ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (200, 120),
      dpr: 1.0,
    })
    .expect("ViewportChanged");

  // Start a navigation, then cancel it by bumping the nav generation once the worker has begun.
  // The delay hook makes the first navigation deterministic by keeping it in-flight long enough
  // for the UI thread to bump the generation.
  let delay_guard = EnvVarGuard::set("FASTR_TEST_RENDER_DELAY_MS", "20");
  cancel.bump_nav();
  ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: "about:newtab".to_string(),
      reason: NavigationReason::TypedUrl,
    })
    .expect("Navigate about:newtab");

  wait_for_navigation_started(&ui_rx, tab_id, "about:newtab", Duration::from_secs(5));

  // Bumping nav cancels both prepare and paint for the in-flight navigation.
  cancel.bump_nav();
  drop(delay_guard);
  ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: "about:blank".to_string(),
      reason: NavigationReason::TypedUrl,
    })
    .expect("Navigate about:blank");

  // The first frame we receive must correspond to about:blank. about:newtab has non-uniform pixels.
  let deadline = Instant::now() + Duration::from_secs(5);
  let frame = loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match ui_rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(WorkerToUi::FrameReady { tab_id: msg_tab, frame }) if msg_tab == tab_id => break frame,
      Ok(WorkerToUi::NavigationFailed {
        tab_id: msg_tab,
        url,
        error,
        ..
      }) if msg_tab == tab_id => {
        panic!("navigation failed for {url}: {error}");
      }
      Ok(_) => {}
      Err(RecvTimeoutError::Timeout) => continue,
      Err(RecvTimeoutError::Disconnected) => panic!("worker disconnected"),
    }
  };

  assert!(
    pixmap_is_uniform_rgba(&frame.pixmap),
    "expected about:blank to render as uniform pixmap; got non-uniform pixels (did cancellation fail?)"
  );

  drop(ui_tx);
  join.join().expect("join worker thread");
}
