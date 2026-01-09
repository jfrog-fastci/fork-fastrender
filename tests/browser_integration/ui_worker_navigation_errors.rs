#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use tempfile::tempdir;
use url::Url;

use super::support::{create_tab_msg, navigate_msg, viewport_changed_msg, DEFAULT_TIMEOUT};

fn recv_until_deadline(rx: &Receiver<WorkerToUi>, deadline: Instant) -> Option<WorkerToUi> {
  loop {
    let now = Instant::now();
    if now >= deadline {
      return None;
    }
    let remaining = deadline - now;
    match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(msg) => return Some(msg),
      Err(RecvTimeoutError::Timeout) => continue,
      Err(RecvTimeoutError::Disconnected) => return None,
    }
  }
}

#[test]
fn missing_file_navigation_emits_navigation_failed_renders_error_frame_and_stops_loading() {
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let missing_path = dir.path().join("missing.html");
  let missing_url = Url::from_file_path(&missing_path)
    .expect("file URL")
    .to_string();

  let (ui_tx, ui_rx, join) = spawn_ui_worker("fastr-ui-worker-missing-file-test")
    .expect("spawn ui worker")
    .split();

  let tab_id = TabId::new();
  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("viewport");
  ui_tx
    .send(navigate_msg(
      tab_id,
      missing_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("send navigate");

  let deadline = Instant::now() + DEFAULT_TIMEOUT;

  let mut saw_started = false;
  let mut saw_loading_true = false;
  let mut saw_failed = false;
  let mut saw_error_frame = false;
  let mut saw_scroll_update = false;
  let mut saw_loading_false = false;

  while !(saw_failed && saw_error_frame && saw_scroll_update && saw_loading_false) {
    let Some(msg) = recv_until_deadline(&ui_rx, deadline) else {
      panic!(
        "timed out waiting for navigation failure flow (started={saw_started}, loading_true={saw_loading_true}, failed={saw_failed}, frame={saw_error_frame}, scroll={saw_scroll_update}, loading_false={saw_loading_false})"
      );
    };

    match msg {
      WorkerToUi::NavigationStarted { tab_id: msg_tab, url } if msg_tab == tab_id => {
        assert_eq!(url, missing_url);
        saw_started = true;
      }
      WorkerToUi::LoadingState {
        tab_id: msg_tab,
        loading,
      } if msg_tab == tab_id => {
        if loading {
          saw_loading_true = true;
        } else {
          saw_loading_false = true;
        }
      }
      WorkerToUi::NavigationFailed {
        tab_id: msg_tab,
        url,
        error,
        ..
      } if msg_tab == tab_id => {
        assert!(saw_started, "expected NavigationStarted before NavigationFailed");
        assert!(saw_loading_true, "expected LoadingState(true) before NavigationFailed");
        assert_eq!(url, missing_url);
        assert!(!error.is_empty(), "expected non-empty error string");
        saw_failed = true;
      }
      WorkerToUi::FrameReady { tab_id: msg_tab, .. } if msg_tab == tab_id => {
        // The worker should render an `about:error` fallback for missing files.
        assert!(saw_failed, "expected NavigationFailed before FrameReady");
        saw_error_frame = true;
      }
      WorkerToUi::ScrollStateUpdated { tab_id: msg_tab, .. } if msg_tab == tab_id => {
        assert!(
          saw_failed,
          "expected NavigationFailed before ScrollStateUpdated for about:error fallback"
        );
        saw_scroll_update = true;
      }
      WorkerToUi::NavigationCommitted { tab_id: msg_tab, .. } if msg_tab == tab_id => {
        panic!("missing-file navigation should not commit");
      }
      _ => {}
    }
  }

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn unknown_about_page_still_commits_and_renders_error_page() {
  let _lock = super::stage_listener_test_lock();
  let (ui_tx, ui_rx, join) = spawn_ui_worker("fastr-ui-worker-unknown-about-test")
    .expect("spawn ui worker")
    .split();

  let tab_id = TabId::new();
  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("viewport");

  let url = "about:does-not-exist".to_string();
  ui_tx
    .send(navigate_msg(tab_id, url.clone(), NavigationReason::TypedUrl))
    .expect("send navigate");

  let deadline = Instant::now() + DEFAULT_TIMEOUT;
  let mut saw_commit = false;
  let mut saw_frame = false;

  while !(saw_commit && saw_frame) {
    let Some(msg) = recv_until_deadline(&ui_rx, deadline) else {
      panic!(
        "timed out waiting for about navigation messages (commit={saw_commit}, frame={saw_frame})"
      );
    };

    match msg {
      WorkerToUi::NavigationCommitted {
        tab_id: msg_tab,
        url: committed,
        ..
      } if msg_tab == tab_id => {
        assert_eq!(committed, url);
        saw_commit = true;
      }
      WorkerToUi::FrameReady { tab_id: msg_tab, .. } if msg_tab == tab_id => {
        saw_frame = true;
      }
      _ => {}
    }
  }

  drop(ui_tx);
  join.join().expect("join ui worker");
}

#[test]
fn missing_file_navigation_renders_about_error_frame_and_updates_nav_flags() {
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let missing_path = dir.path().join("missing.html");
  let missing_url = Url::from_file_path(&missing_path)
    .expect("file URL")
    .to_string();

  let worker = spawn_ui_worker("fastr-ui-worker-missing-file-history-test")
    .expect("spawn ui worker");

  let tab_id = TabId::new();
  worker
    .ui_tx
    .send(create_tab_msg(tab_id, Some("about:newtab".to_string())))
    .expect("create tab");
  worker
    .ui_tx
    .send(viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("viewport");

  // Wait for the initial about:newtab navigation to paint so history exists.
  super::support::recv_for_tab(&worker.ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .expect("initial FrameReady");

  // Navigate to a file:// URL that does not exist.
  worker
    .ui_tx
    .send(navigate_msg(
      tab_id,
      missing_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate missing file");

  let deadline = Instant::now() + DEFAULT_TIMEOUT;
  let mut saw_frame = false;
  let mut saw_scroll = false;
  let mut saw_failed = false;

  while Instant::now() < deadline {
    let Some(msg) = recv_until_deadline(&worker.ui_rx, deadline) else {
      break;
    };

    match msg {
      WorkerToUi::NavigationFailed {
        tab_id: msg_tab,
        url,
        error,
        can_go_back,
        can_go_forward,
        ..
      } if msg_tab == tab_id => {
        if url != missing_url {
          continue;
        }
        assert!(!error.is_empty(), "expected non-empty error string");
        assert!(
          can_go_back,
          "expected can_go_back=true after failed navigation from about:newtab"
        );
        assert!(
          !can_go_forward,
          "expected can_go_forward=false after a new failing navigation"
        );
        saw_failed = true;
      }
      WorkerToUi::FrameReady {
        tab_id: msg_tab,
        frame,
      } if msg_tab == tab_id => {
        if !saw_failed {
          // The fallback error page should only be painted after we report the failure.
          continue;
        }
        assert!(
          frame.pixmap.width() > 0 && frame.pixmap.height() > 0,
          "expected non-empty pixmap for about:error fallback"
        );
        saw_frame = true;
      }
      WorkerToUi::ScrollStateUpdated { tab_id: msg_tab, .. } if msg_tab == tab_id => {
        if saw_failed {
          saw_scroll = true;
        }
      }
      WorkerToUi::NavigationCommitted {
        tab_id: msg_tab,
        url,
        ..
      } if msg_tab == tab_id => {
        if url == missing_url {
          panic!("missing-file navigation should not commit");
        }
      }
      _ => {}
    }

    if saw_failed && saw_frame && saw_scroll {
      break;
    }
  }

  assert!(saw_failed, "expected NavigationFailed for missing file");
  assert!(saw_frame, "expected about:error fallback FrameReady after failure");
  assert!(
    saw_scroll,
    "expected ScrollStateUpdated for the about:error fallback frame"
  );

  worker.join().expect("join ui worker");
}
