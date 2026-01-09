#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::scroll::ScrollState;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_browser_worker;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(10);

struct BrowserWorkerFixture {
  tx: Option<Sender<UiToWorker>>,
  rx: Receiver<WorkerToUi>,
  join: Option<std::thread::JoinHandle<()>>,
}

impl BrowserWorkerFixture {
  fn join_with_timeout(join: std::thread::JoinHandle<()>, timeout: Duration) {
    let (done_tx, done_rx) = std::sync::mpsc::channel::<std::thread::Result<()>>();
    std::thread::spawn(move || {
      let _ = done_tx.send(join.join());
    });
    match done_rx.recv_timeout(timeout) {
      Ok(res) => res.expect("join browser worker thread"),
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
        panic!("timed out joining browser worker thread");
      }
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
        panic!("join waiter disconnected");
      }
    }
  }

  fn spawn() -> Self {
    let handle = spawn_browser_worker().expect("spawn browser worker");
    Self {
      tx: Some(handle.tx),
      rx: handle.rx,
      join: Some(handle.join),
    }
  }

  fn tx(&self) -> &Sender<UiToWorker> {
    self.tx.as_ref().expect("worker tx available")
  }

  fn shutdown(mut self) {
    let _ = self.tx.take();
    if let Some(join) = self.join.take() {
      // Joining can hang if the worker is wedged (e.g. a rendering deadlock). Fail fast so CI
      // doesn't stall indefinitely on a single test.
      Self::join_with_timeout(join, Duration::from_secs(5));
    }
  }
}

impl Drop for BrowserWorkerFixture {
  fn drop(&mut self) {
    let _ = self.tx.take();
    if let Some(join) = self.join.take() {
      // Never block on drop: if a test panics while the worker is stuck, joining would hang the
      // whole test binary. Best-effort: join on a detached helper thread.
      std::thread::spawn(move || {
        let _ = join.join();
      });
    }
  }
}

fn wait_for_navigation_complete(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  timeout: Duration,
) -> String {
  let deadline = Instant::now() + timeout;
  let mut msgs: Vec<WorkerToUi> = Vec::new();
  while Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let slice = remaining.min(Duration::from_millis(25));
    let msg = match rx.recv_timeout(slice) {
      Ok(msg) => msg,
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    };
    msgs.push(msg);
    let last = msgs.len() - 1;
    match &msgs[last] {
      WorkerToUi::NavigationCommitted { tab_id: got, url, .. } if *got == tab_id => {
        return url.clone();
      }
      WorkerToUi::NavigationFailed {
        tab_id: got,
        url,
        error,
        ..
      } if *got == tab_id => {
        panic!("navigation failed for {url}: {error}");
      }
      _ => {}
    }
  }

  panic!(
    "timed out waiting for NavigationCommitted for tab {tab_id:?}\nmessages:\n{}",
    support::format_messages(&msgs)
  );
}

fn wait_for_frame(rx: &Receiver<WorkerToUi>, tab_id: TabId, timeout: Duration) -> fastrender::ui::messages::RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, timeout, |msg| matches!(msg, WorkerToUi::FrameReady { .. }))
    .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));
  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn wait_for_loading_false(rx: &Receiver<WorkerToUi>, tab_id: TabId, timeout: Duration) {
  let msg = support::recv_for_tab(rx, tab_id, timeout, |msg| {
    matches!(msg, WorkerToUi::LoadingState { loading: false, .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for LoadingState(false) for tab {tab_id:?}"));
  match msg {
    WorkerToUi::LoadingState { loading: false, .. } => {}
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn drain_available(rx: &Receiver<WorkerToUi>) {
  while rx.try_recv().is_ok() {}
}

fn heavy_file_html(rows: usize) -> String {
  // Deterministic (offline) heavy page for cancellation tests. This mirrors the built-in
  // `about:test-heavy` fixture but is served via file:// to exercise the production fetch+prepare
  // path.
  let mut out = String::with_capacity(rows * 64);
  out.push_str(
    "<!doctype html><html><head><meta charset=\"utf-8\"><title>Heavy File</title>\
     <style>body{margin:0;font:14px/1.3 system-ui, -apple-system, Segoe UI, sans-serif;}\
     .row{padding:4px 8px;border-bottom:1px solid rgba(0,0,0,0.08);}</style>\
     </head><body>",
  );
  for i in 0..rows {
    use std::fmt::Write;
    let _ = write!(out, "<div class=\"row\">row {i}</div>");
  }
  out.push_str("</body></html>");
  out
}

#[test]
fn create_tab_triggers_initial_navigation_and_frame() {
  let _lock = super::stage_listener_test_lock();
  let worker = BrowserWorkerFixture::spawn();

  let tab_id = TabId::new();
  worker
    .tx()
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: CancelGens::new(),
    })
    .expect("CreateTab");

  let msg1 = worker
    .rx
    .recv_timeout(TIMEOUT)
    .unwrap_or_else(|err| panic!("timed out waiting for initial WorkerToUi message: {err}"));
  match msg1 {
    WorkerToUi::LoadingState { tab_id: got, loading } => {
      assert_eq!(got, tab_id);
      assert!(loading, "expected LoadingState(true)");
    }
    other => panic!("expected LoadingState(true) first, got {other:?}"),
  }

  let msg2 = worker
    .rx
    .recv_timeout(TIMEOUT)
    .unwrap_or_else(|err| panic!("timed out waiting for NavigationStarted: {err}"));
  match msg2 {
    WorkerToUi::NavigationStarted { tab_id: got, url } => {
      assert_eq!(got, tab_id);
      assert_eq!(url, "about:newtab");
    }
    other => panic!("expected NavigationStarted second, got {other:?}"),
  }

  let committed_url = wait_for_navigation_complete(&worker.rx, tab_id, TIMEOUT);
  assert_eq!(committed_url, "about:newtab");
  let _ = wait_for_frame(&worker.rx, tab_id, TIMEOUT);
  wait_for_loading_false(&worker.rx, tab_id, TIMEOUT);

  worker.shutdown();
}

#[test]
fn scroll_produces_scroll_update_and_frame() {
  let _lock = super::stage_listener_test_lock();
  let worker = BrowserWorkerFixture::spawn();

  let tab_id = TabId::new();
  worker
    .tx()
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: Some("about:test-scroll".to_string()),
      cancel: CancelGens::new(),
    })
    .expect("CreateTab");

  let committed_url = wait_for_navigation_complete(&worker.rx, tab_id, TIMEOUT);
  assert_eq!(committed_url, "about:test-scroll");
  let _ = wait_for_frame(&worker.rx, tab_id, TIMEOUT);
  wait_for_loading_false(&worker.rx, tab_id, TIMEOUT);
  drain_available(&worker.rx);

  worker
    .tx()
    .send(UiToWorker::Scroll {
      tab_id,
      delta_css: (0.0, 200.0),
      pointer_css: None,
    })
    .expect("Scroll");

  let deadline = Instant::now() + TIMEOUT;
  let mut scroll_state: Option<ScrollState> = None;
  let mut frame_scroll_state: Option<ScrollState> = None;
  while Instant::now() < deadline && (scroll_state.is_none() || frame_scroll_state.is_none()) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let msg = match worker.rx.recv_timeout(remaining) {
      Ok(msg) => msg,
      Err(_) => break,
    };
    match msg {
      WorkerToUi::ScrollStateUpdated { tab_id: got, scroll } if got == tab_id => {
        scroll_state = Some(scroll);
      }
      WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => {
        frame_scroll_state = Some(frame.scroll_state);
      }
      WorkerToUi::NavigationFailed { url, error, .. } => {
        panic!("navigation failed for {url}: {error}");
      }
      _ => {}
    }
  }

  let scroll_state = scroll_state.unwrap_or_else(|| panic!("expected ScrollStateUpdated after scroll"));
  let frame_scroll_state =
    frame_scroll_state.unwrap_or_else(|| panic!("expected FrameReady after scroll"));

  assert!(
    scroll_state.viewport.y > 0.0,
    "expected scroll.viewport.y > 0, got {:?}",
    scroll_state.viewport
  );
  assert!(
    (frame_scroll_state.viewport.y - scroll_state.viewport.y).abs() < 1e-3,
    "expected FrameReady.scroll_state.viewport.y ({}) to match ScrollStateUpdated ({})",
    frame_scroll_state.viewport.y,
    scroll_state.viewport.y
  );

  worker.shutdown();
}

#[test]
fn cancellation_drops_stale_output() {
  let _lock = super::stage_listener_test_lock();
  let worker = BrowserWorkerFixture::spawn();

  let tab_id = TabId::new();
  let cancel = CancelGens::new();
  worker
    .tx()
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: cancel.clone(),
    })
    .expect("CreateTab");

  let _ = wait_for_navigation_complete(&worker.rx, tab_id, TIMEOUT);
  let _ = wait_for_frame(&worker.rx, tab_id, TIMEOUT);
  wait_for_loading_false(&worker.rx, tab_id, TIMEOUT);
  drain_available(&worker.rx);

  let site = support::TempSite::new();
  let heavy_url = site.write("heavy.html", &heavy_file_html(50_000));
  let cheap_url = "about:blank".to_string();

  worker
    .tx()
    .send(UiToWorker::Navigate {
      tab_id,
      url: heavy_url.clone(),
      reason: NavigationReason::TypedUrl,
    })
    .expect("Navigate heavy");

  // Wait for evidence that the heavy navigation is in-flight.
  support::recv_for_tab(&worker.rx, tab_id, TIMEOUT, |msg| matches!(msg, WorkerToUi::Stage { .. }))
    .unwrap_or_else(|| panic!("timed out waiting for Stage heartbeat during heavy navigation"));

  // Cancel the in-flight job from the UI side while the worker is blocked in `prepare_url`.
  cancel.bump_nav();
  worker
    .tx()
    .send(UiToWorker::Navigate {
      tab_id,
      url: cheap_url.clone(),
      reason: NavigationReason::TypedUrl,
    })
    .expect("Navigate cheap");

  let deadline = Instant::now() + Duration::from_secs(20);
  let mut last_committed: Option<String> = None;
  let mut saw_cheap_commit = false;
  let mut saw_cheap_frame = false;
  let mut saw_cheap_loading_false = false;
  let mut msgs: Vec<WorkerToUi> = Vec::new();

  while Instant::now() < deadline && !(saw_cheap_commit && saw_cheap_frame && saw_cheap_loading_false) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let msg = match worker.rx.recv_timeout(remaining) {
      Ok(msg) => msg,
      Err(_) => break,
    };
    msgs.push(msg);
    let last = msgs.len() - 1;
    match &msgs[last] {
      WorkerToUi::NavigationCommitted { url, .. } => {
        if url == &heavy_url {
          panic!(
            "observed NavigationCommitted for cancelled heavy URL after cancellation\nmessages:\n{}",
            support::format_messages(&msgs)
          );
        }
        if url == &cheap_url {
          saw_cheap_commit = true;
        }
        last_committed = Some(url.clone());
      }
      WorkerToUi::NavigationFailed { url, error, .. } => {
        if url == &heavy_url {
          panic!(
            "observed NavigationFailed for cancelled heavy URL ({error})\nmessages:\n{}",
            support::format_messages(&msgs)
          );
        }
        if url == &cheap_url {
          panic!(
            "cheap navigation unexpectedly failed ({error})\nmessages:\n{}",
            support::format_messages(&msgs)
          );
        }
      }
      WorkerToUi::FrameReady { frame, .. } => {
        if last_committed.as_deref() == Some(cheap_url.as_str()) {
          saw_cheap_frame = true;
        } else if last_committed.as_deref() == Some(heavy_url.as_str()) {
          panic!(
            "observed FrameReady for cancelled heavy URL\nmessages:\n{}",
            support::format_messages(&msgs)
          );
        } else {
          // Ignore: could be an initial frame emitted before we observed the cheap commit.
          let _ = frame;
        }
      }
      WorkerToUi::LoadingState { loading, .. } => {
        if !*loading && last_committed.as_deref() == Some(cheap_url.as_str()) {
          saw_cheap_loading_false = true;
        }
      }
      _ => {}
    }
  }

  assert!(
    saw_cheap_commit,
    "expected NavigationCommitted for cheap URL ({cheap_url})\nmessages:\n{}",
    support::format_messages(&msgs)
  );
  assert!(
    saw_cheap_frame,
    "expected FrameReady for cheap URL ({cheap_url})\nmessages:\n{}",
    support::format_messages(&msgs)
  );
  assert!(
    saw_cheap_loading_false,
    "expected LoadingState(false) after cheap navigation\nmessages:\n{}",
    support::format_messages(&msgs)
  );

  // Ensure the cancelled heavy navigation doesn't publish output after the cheap commit.
  let tail = support::drain_for(&worker.rx, Duration::from_millis(250));
  let saw_heavy_output = tail.iter().any(|msg| match msg {
    WorkerToUi::NavigationCommitted { url, .. } | WorkerToUi::NavigationFailed { url, .. } => url == &heavy_url,
    _ => false,
  });
  if saw_heavy_output {
    let mut combined = msgs;
    combined.extend(tail);
    panic!(
      "observed stale output for cancelled heavy navigation after cheap commit\nmessages:\n{}",
      support::format_messages(&combined)
    );
  }

  worker.shutdown();
}
