#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use tempfile::tempdir;

use super::support;

#[test]
fn navigation_invalid_url_emits_navigation_failed() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-navigation-invalid-url-test",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");

  let url = "foo://example.com";
  let tab_id = TabId(1);
  handle
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("send CreateTab");
  handle
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (32, 32), 1.0))
    .expect("send ViewportChanged");
  handle
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      url.to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("send Navigate");

  let msg = support::recv_for_tab(&handle.ui_rx, tab_id, support::DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::NavigationFailed { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationFailed for {url:?}"));

  let error = match msg {
    WorkerToUi::NavigationFailed {
      url: msg_url,
      error,
      ..
    } => {
      assert_eq!(msg_url, url);
      error
    }
    other => panic!("expected NavigationFailed message, got {other:?}"),
  };
  assert!(
    !error.as_str().trim().is_empty(),
    "expected non-empty NavigationFailed error string"
  );

  handle.join().expect("join ui worker");
}

#[test]
fn navigation_file_url_emits_started_committed_and_loading_toggle() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let html_path = dir.path().join("index.html");
  std::fs::write(
    &html_path,
    "<!doctype html><html><head><title>Hello</title></head><body>Hi</body></html>",
  )
  .expect("write html");

  let url = url::Url::from_file_path(&html_path).unwrap().to_string();
  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-navigation-file-url-test",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let tab_id = TabId(1);
  handle
    .ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("send CreateTab");
  handle
    .ui_tx
    .send(support::viewport_changed_msg(tab_id, (32, 32), 1.0))
    .expect("send ViewportChanged");

  handle
    .ui_tx
    .send(support::navigate_msg(
      tab_id,
      url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("send Navigate");

  // Collect messages until the navigation finishes (LoadingState(false)).
  let deadline = std::time::Instant::now() + support::DEFAULT_TIMEOUT;
  let mut messages = Vec::new();
  loop {
    let now = std::time::Instant::now();
    if now >= deadline {
      panic!(
        "timed out waiting for navigation completion; got:\n{}",
        support::format_messages(&messages)
      );
    }
    let remaining = deadline.saturating_duration_since(now);
    match handle
      .ui_rx
      .recv_timeout(remaining.min(std::time::Duration::from_millis(100)))
    {
      Ok(msg) => {
        messages.push(msg);
        if matches!(
          messages.last(),
          Some(WorkerToUi::LoadingState {
            tab_id: got,
            loading: false,
          }) if *got == tab_id
        ) {
          break;
        }
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  let mut started_idx = None;
  let mut committed_idx = None;
  let mut loading_true_idx = None;
  let mut loading_false_idx = None;

  for (idx, msg) in messages.iter().enumerate() {
    match msg {
      WorkerToUi::NavigationStarted { url: msg_url, .. } if msg_url == &url => {
        started_idx.get_or_insert(idx);
      }
      WorkerToUi::NavigationCommitted {
        url: msg_url,
        title,
        can_go_back,
        can_go_forward,
        ..
      } if msg_url == &url => {
        committed_idx.get_or_insert(idx);
        assert_eq!(title.as_deref(), Some("Hello"));
        // This is the first committed history entry for a tab created without an initial URL.
        assert!(!*can_go_back);
        assert!(!can_go_forward);
      }
      WorkerToUi::LoadingState {
        tab_id: got,
        loading: true,
      } if *got == tab_id => {
        loading_true_idx.get_or_insert(idx);
      }
      WorkerToUi::LoadingState {
        tab_id: got,
        loading: false,
      } if *got == tab_id => {
        loading_false_idx.get_or_insert(idx);
      }
      _ => {}
    }
  }

  let started_idx = started_idx.unwrap_or_else(|| {
    panic!("expected NavigationStarted for {url:?}, got {messages:?}");
  });
  let committed_idx = committed_idx.unwrap_or_else(|| {
    panic!("expected NavigationCommitted for {url:?}, got {messages:?}");
  });
  assert!(
    started_idx < committed_idx,
    "expected NavigationStarted before NavigationCommitted"
  );

  let loading_true_idx = loading_true_idx.unwrap_or_else(|| {
    panic!("expected LoadingState {{ loading: true }} message, got {messages:?}");
  });
  let loading_false_idx = loading_false_idx.unwrap_or_else(|| {
    panic!("expected LoadingState {{ loading: false }} message, got {messages:?}");
  });
  assert!(
    loading_true_idx < loading_false_idx,
    "expected LoadingState true before false"
  );

  handle.join().expect("join ui worker");
}
