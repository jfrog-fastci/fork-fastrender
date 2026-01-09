#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::worker_loop::spawn_ui_worker;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use tempfile::tempdir;
use url::Url;

use super::support::{create_tab_msg, navigate_msg};

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
fn missing_file_navigation_emits_navigation_failed_and_stops_loading() {
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let missing_path = dir.path().join("missing.html");
  let missing_url = Url::from_file_path(&missing_path)
    .expect("file URL")
    .to_string();

  let handle = spawn_ui_worker("fastr-ui-worker-loop-missing-file-test").expect("spawn worker loop");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId(1);
  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("create tab");
  ui_tx
    .send(navigate_msg(
      tab_id,
      missing_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("send navigate");

  #[derive(Debug)]
  enum Expect {
    Started,
    LoadingTrue,
    Failed,
    LoadingFalse,
    Done,
  }

  let mut expect = Expect::Started;
  let mut saw_frame = false;
  let deadline = Instant::now() + Duration::from_secs(10);

  while !matches!(expect, Expect::Done) {
    let Some(msg) = recv_until_deadline(&ui_rx, deadline) else {
      panic!("timed out waiting for navigation messages; last state: {expect:?}");
    };

    match msg {
      WorkerToUi::NavigationStarted {
        tab_id: msg_tab,
        url,
      } if msg_tab == tab_id => {
        assert!(
          matches!(expect, Expect::Started),
          "NavigationStarted out of order: {expect:?}"
        );
        assert_eq!(url, missing_url);
        expect = Expect::LoadingTrue;
      }
      WorkerToUi::LoadingState { tab_id: msg_tab, loading } if msg_tab == tab_id => {
        if loading {
          assert!(
            matches!(expect, Expect::LoadingTrue),
            "LoadingState(true) out of order: {expect:?}"
          );
          expect = Expect::Failed;
        } else {
          assert!(
            matches!(expect, Expect::LoadingFalse),
            "LoadingState(false) out of order: {expect:?}"
          );
          expect = Expect::Done;
        }
      }
      WorkerToUi::NavigationFailed {
        tab_id: msg_tab,
        url,
        error,
      } if msg_tab == tab_id => {
        assert!(
          matches!(expect, Expect::Failed),
          "NavigationFailed out of order: {expect:?}"
        );
        assert_eq!(url, missing_url);
        assert!(!error.is_empty(), "expected non-empty error string");
        expect = Expect::LoadingFalse;
      }
      WorkerToUi::FrameReady { tab_id: msg_tab, .. } => {
        assert_eq!(msg_tab, tab_id, "FrameReady should be scoped to the navigating tab");
        saw_frame = true;
      }
      WorkerToUi::NavigationCommitted { tab_id: msg_tab, .. } if msg_tab == tab_id => {
        panic!("missing-file navigation should not commit");
      }
      _ => {}
    }
  }

  // Implementation may render an `about:error` page after failure. If a frame is produced, it must
  // be scoped to the correct tab id.
  if saw_frame {
    // Already asserted above.
  }

  drop(ui_tx);
  join.join().expect("join worker loop");
}

#[test]
fn unknown_about_page_still_commits_and_renders_error_page() {
  let _lock = super::stage_listener_test_lock();
  let handle = spawn_ui_worker("fastr-ui-worker-loop-unknown-about-test").expect("spawn worker loop");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId(1);
  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("create tab");

  let url = "about:does-not-exist".to_string();
  ui_tx
    .send(navigate_msg(tab_id, url.clone(), NavigationReason::TypedUrl))
    .expect("send navigate");

  let deadline = Instant::now() + Duration::from_secs(10);
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
  join.join().expect("join worker loop");
}
