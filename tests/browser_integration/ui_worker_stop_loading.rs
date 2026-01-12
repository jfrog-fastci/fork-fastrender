#![cfg(feature = "browser_ui")]

use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker_for_test;
use std::time::{Duration, Instant};

use super::support::{create_tab_msg_with_cancel, navigate_msg, recv_for_tab, viewport_changed_msg, DEFAULT_TIMEOUT};

#[test]
fn stop_loading_cancels_in_flight_navigation_and_restores_committed_history_entry() {
  let _lock = super::stage_listener_test_lock();

  // Slow down rendering so the heavy page stays "loading" long enough for us to stop it.
  let (ui_tx, ui_rx, join) =
    spawn_ui_worker_for_test("fastr-ui-worker-stop-loading", Some(5))
      .expect("spawn ui worker")
      .split();

  let tab_id = TabId::new();
  let cancel = CancelGens::new();

  ui_tx
    .send(create_tab_msg_with_cancel(
      tab_id,
      Some("about:newtab".to_string()),
      cancel.clone(),
    ))
    .expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (240, 160), 1.0))
    .expect("ViewportChanged");

  recv_for_tab(&ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationCommitted { url, .. } if url == "about:newtab"
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for about:newtab NavigationCommitted"));

  let target = "about:test-heavy".to_string();

  // Start a slow navigation.
  cancel.bump_nav();
  ui_tx
    .send(navigate_msg(tab_id, target.clone(), NavigationReason::TypedUrl))
    .expect("Navigate about:test-heavy");

  // Ensure the worker has observed the navigation before issuing StopLoading.
  recv_for_tab(&ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationStarted { url, .. } if url == &target
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationStarted for {target:?}"));

  // Cancel the in-flight navigation (mimic the windowed UI behaviour: bump gens first).
  cancel.bump_nav();
  ui_tx
    .send(UiToWorker::StopLoading { tab_id })
    .expect("StopLoading");

  let mut messages = Vec::new();
  let deadline = Instant::now() + DEFAULT_TIMEOUT;
  let mut saw_loading_false = false;
  let mut saw_committed_target = false;
  let mut saw_restored_commit = None;

  // `StopLoading` should immediately clear loading and then re-emit a NavigationCommitted for the
  // still-current committed entry so the chrome can restore URL/title/back/forward flags.
  while Instant::now() < deadline && saw_restored_commit.is_none() {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match ui_rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
      Ok(msg) => {
        match &msg {
          WorkerToUi::LoadingState {
            tab_id: got,
            loading: false,
          } if *got == tab_id => {
            saw_loading_false = true;
          }
          WorkerToUi::NavigationCommitted { url, .. } if url == &target => {
            saw_committed_target = true;
          }
          WorkerToUi::NavigationCommitted {
            url,
            can_go_back,
            can_go_forward,
            ..
          } if url == "about:newtab" => {
            saw_restored_commit = Some((*can_go_back, *can_go_forward));
          }
          _ => {}
        }
        messages.push(msg);
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  assert!(
    saw_loading_false,
    "expected LoadingState {{ loading: false }} after StopLoading; got:\n{}",
    super::support::format_messages(&messages)
  );
  assert!(
    !saw_committed_target,
    "unexpected NavigationCommitted for cancelled URL {target:?}; got:\n{}",
    super::support::format_messages(&messages)
  );
  let (can_go_back, can_go_forward) = saw_restored_commit.unwrap_or_else(|| {
    panic!(
      "expected NavigationCommitted restoring about:newtab after StopLoading; got:\n{}",
      super::support::format_messages(&messages)
    )
  });
  assert!(
    !can_go_back && !can_go_forward,
    "expected history to be reset to only about:newtab (no back/forward); got back={can_go_back} forward={can_go_forward}"
  );

  // A subsequent Reload must reload the still-current (committed) URL, not the cancelled one.
  cancel.bump_nav();
  ui_tx
    .send(UiToWorker::Reload { tab_id })
    .expect("Reload");
  let reload_started = recv_for_tab(&ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::NavigationStarted { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationStarted after Reload"));

  let reload_url = match reload_started {
    WorkerToUi::NavigationStarted { url, .. } => url,
    other => panic!("expected NavigationStarted, got {other:?}"),
  };
  assert_eq!(
    reload_url, "about:newtab",
    "expected Reload after StopLoading to reload about:newtab, got {reload_url:?}"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
