#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
use std::ffi::OsString;
use std::time::{Duration, Instant};

const MAX_WAIT: Duration = Duration::from_secs(15);

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

#[test]
fn navigation_cancellation_drops_stale_frame_and_is_silent() {
  let _lock = super::stage_listener_test_lock();
  let _env = EnvVarGuard::set("FASTR_TEST_RENDER_DELAY_MS", "1");

  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let tab_id = TabId::new();
  let cancel = CancelGens::new();
  worker
    .tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: cancel.clone(),
    })
    .expect("create tab");
  worker
    .tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (128, 128),
      dpr: 1.0,
    })
    .expect("viewport");

  let first_url = "about:test-heavy".to_string();
  let second_url = "about:blank".to_string();

  worker
    .tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: first_url.clone(),
      reason: NavigationReason::TypedUrl,
    })
    .expect("navigate first");

  let deadline = Instant::now() + MAX_WAIT;
  let mut started_first = false;
  let mut sent_second = false;
  let mut last_committed: Option<String> = None;
  let mut saw_second_frame = false;
  let mut saw_first_frame = false;
  let mut saw_failed_first = false;
  let mut saw_debug_log = false;
  let mut captured: Vec<WorkerToUi> = Vec::new();

  while Instant::now() < deadline {
    match worker.rx.recv_timeout(Duration::from_millis(200)) {
      Ok(msg) => {
        match &msg {
          WorkerToUi::NavigationStarted { tab_id: got, url } if *got == tab_id && url == &first_url => {
            started_first = true;
          }
          WorkerToUi::Stage { tab_id: got, .. } if *got == tab_id && started_first && !sent_second => {
            // Simulate UI-driven cancellation while the worker is blocked in the first navigation.
            cancel.bump_nav();
            worker
              .tx
              .send(UiToWorker::Navigate {
                tab_id,
                url: second_url.clone(),
                reason: NavigationReason::TypedUrl,
              })
              .expect("navigate second");
            sent_second = true;
          }
          WorkerToUi::NavigationCommitted { tab_id: got, url, .. } if *got == tab_id => {
            last_committed = Some(url.clone());
          }
          WorkerToUi::NavigationFailed { tab_id: got, url, .. } if *got == tab_id => {
            if url == &first_url {
              saw_failed_first = true;
            }
          }
          WorkerToUi::DebugLog { tab_id: got, .. } if *got == tab_id => {
            saw_debug_log = true;
          }
          WorkerToUi::FrameReady { tab_id: got, .. } if *got == tab_id => {
            if last_committed.as_deref() == Some(second_url.as_str()) {
              saw_second_frame = true;
              captured.push(msg);
              break;
            }
            if last_committed.as_deref() == Some(first_url.as_str()) {
              saw_first_frame = true;
              captured.push(msg);
              break;
            }
          }
          _ => {}
        }
        captured.push(msg);
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  drop(worker.tx);
  worker.join.join().expect("worker join");

  assert!(
    started_first,
    "expected to observe NavigationStarted for the first URL; messages:\n{}",
    support::format_messages(&captured)
  );
  assert!(
    sent_second,
    "expected to observe a stage heartbeat during the first navigation; messages:\n{}",
    support::format_messages(&captured)
  );
  assert!(
    saw_second_frame,
    "expected FrameReady for the second navigation; messages:\n{}",
    support::format_messages(&captured)
  );
  assert!(
    !saw_first_frame,
    "expected no FrameReady for the cancelled first navigation; messages:\n{}",
    support::format_messages(&captured)
  );
  assert!(
    !saw_failed_first,
    "expected no NavigationFailed for the cancelled first navigation; messages:\n{}",
    support::format_messages(&captured)
  );
  assert!(
    !saw_debug_log,
    "expected no DebugLog during cancellation; messages:\n{}",
    support::format_messages(&captured)
  );
}

#[test]
fn rapid_scroll_cancels_stale_paint() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();

  let mut body = String::new();
  // Keep the document big enough to scroll, but not so large that test render delay hooks make
  // the initial frame excessively slow under CI load.
  for i in 0..16 {
    body.push_str(&format!("<div class=\"row\">row {i}</div>\n"));
  }
  let url = site.write(
    "scroll.html",
    &format!(
      r#"<!doctype html>
        <html>
          <head>
            <style>
              html, body {{ margin: 0; padding: 0; }}
              .row {{ height: 40px; border-bottom: 1px solid #ccc; }}
            </style>
          </head>
          <body>
            {body}
          </body>
        </html>"#
    ),
  );

  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let tab_id = TabId::new();
  let cancel = CancelGens::new();
  worker
    .tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: Some(url),
      cancel: cancel.clone(),
    })
    .expect("create tab");
  worker
    .tx
    .send(UiToWorker::SetActiveTab { tab_id })
    .expect("set active tab");
  worker
    .tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (200, 120),
      dpr: 1.0,
    })
    .expect("viewport");

  // Wait for the initial frame so we have layout cache and a document installed.
  let deadline = Instant::now() + MAX_WAIT;
  let mut initial_trace: Vec<WorkerToUi> = Vec::new();
  let mut saw_initial_frame = false;
  while Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match worker.rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(msg) => {
        if matches!(msg, WorkerToUi::FrameReady { tab_id: got, .. } if got == tab_id) {
          saw_initial_frame = true;
          initial_trace.push(msg);
          break;
        }
        initial_trace.push(msg);
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }
  assert!(
    saw_initial_frame,
    "initial frame\nmessages:\n{}",
    support::format_messages(&initial_trace)
  );
  let _ = support::drain_for(&worker.rx, Duration::from_millis(100));

  // Enable an artificial render delay for the scroll paints only. This keeps the initial navigation
  // fast (so we don't flake on the first frame) while ensuring the scroll paint is slow enough that
  // we can deterministically cancel it after receiving a stage heartbeat.
  let _env = EnvVarGuard::set("FASTR_TEST_RENDER_DELAY_MS", "1");

  worker
    .tx
    .send(UiToWorker::Scroll {
      tab_id,
      delta_css: (0.0, 80.0),
      pointer_css: None,
    })
    .expect("scroll 1");

  // Wait until we see at least one stage heartbeat during the first scroll paint, then cancel it.
  let _ = support::recv_for_tab(&worker.rx, tab_id, MAX_WAIT, |msg| {
    matches!(msg, WorkerToUi::Stage { .. })
  })
  .expect("stage heartbeat during scroll paint");

  cancel.bump_paint();
  worker
    .tx
    .send(UiToWorker::Scroll {
      tab_id,
      delta_css: (0.0, 80.0),
      pointer_css: None,
    })
    .expect("scroll 2");

  let deadline = Instant::now() + MAX_WAIT;
  let mut frames = Vec::new();
  let mut captured = Vec::new();

  while Instant::now() < deadline {
    match worker.rx.recv_timeout(Duration::from_millis(200)) {
      Ok(msg) => {
        match &msg {
          WorkerToUi::FrameReady { tab_id: got, frame } if *got == tab_id => {
            frames.push(frame.scroll_state.clone());
            captured.push(msg);
            break;
          }
          WorkerToUi::DebugLog { tab_id: got, .. } if *got == tab_id => {
            captured.push(msg);
            break;
          }
          _ => {}
        }
        captured.push(msg);
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  // Ensure a stale frame does not arrive after the latest scroll frame.
  captured.extend(support::drain_for(&worker.rx, Duration::from_secs(1)));

  drop(worker.tx);
  worker.join.join().expect("worker join");

  assert!(
    captured
      .iter()
      .all(|msg| !matches!(msg, WorkerToUi::DebugLog { tab_id: got, .. } if *got == tab_id)),
    "expected no DebugLog during scroll cancellation; messages:\n{}",
    support::format_messages(&captured)
  );

  assert_eq!(
    frames.len(),
    1,
    "expected exactly one FrameReady after scroll cancellation; messages:\n{}",
    support::format_messages(&captured)
  );

  let frame_scroll = &frames[0];
  assert!(
    (frame_scroll.viewport.y - 160.0).abs() < 0.5,
    "expected painted scroll_y ~= 160, got {:?}; messages:\n{}",
    frame_scroll.viewport,
    support::format_messages(&captured)
  );

  let latest = captured
    .iter()
    .rev()
    .find_map(|msg| match msg {
      WorkerToUi::ScrollStateUpdated { tab_id: got, scroll } if *got == tab_id => Some(scroll.clone()),
      _ => None,
    })
    .unwrap_or_else(|| {
      panic!(
        "expected at least one ScrollStateUpdated; messages:\n{}",
        support::format_messages(&captured)
      )
    });
  assert_eq!(
    latest.viewport, frame_scroll.viewport,
    "expected FrameReady scroll_state to match ScrollStateUpdated; messages:\n{}",
    support::format_messages(&captured)
  );

  assert!(
    captured
      .iter()
      .filter(|msg| matches!(msg, WorkerToUi::FrameReady { tab_id: got, .. } if *got == tab_id))
      .count()
      == 1,
    "unexpected additional FrameReady messages after latest scroll frame; messages:\n{}",
    support::format_messages(&captured)
  );
}
