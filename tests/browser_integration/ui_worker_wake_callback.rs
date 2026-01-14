use fastrender::ui::{
  spawn_browser_ui_worker, TabId, UiToWorker, WorkerToUi, WorkerToUiInbox, WorkerWakeCallback,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
fn ui_worker_wake_callback_invoked_on_successful_send() {
  let _lock = super::stage_listener_test_lock();

  let calls = Arc::new(AtomicUsize::new(0));
  let wake: WorkerWakeCallback = {
    let calls = Arc::clone(&calls);
    Arc::new(move || {
      calls.fetch_add(1, Ordering::Relaxed);
    })
  };

  let (ui_tx, ui_rx_raw, join) =
    spawn_browser_ui_worker("ui-worker-wake-callback-ok", Some(wake))
      .expect("spawn_browser_ui_worker");
  let ui_rx = WorkerToUiInbox::new(ui_rx_raw);

  let tab_id = TabId::new();
  ui_tx
    .send(UiToWorker::SelectDropdownCancel { tab_id })
    .expect("send cancel");

  let deadline = Instant::now() + Duration::from_secs(2);
  let mut saw_expected = false;
  while Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match ui_rx.recv_timeout(remaining) {
      Ok(WorkerToUi::SelectDropdownClosed { tab_id: msg_tab }) if msg_tab == tab_id => {
        saw_expected = true;
        break;
      }
      Ok(_) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  assert!(saw_expected, "expected SelectDropdownClosed from worker");

  // The wake callback is invoked immediately after successful enqueue, but another thread can
  // observe the channel receive before the worker thread runs the callback. Give it a short grace
  // period to avoid flakes under heavy scheduling contention.
  let grace_deadline = Instant::now() + Duration::from_millis(200);
  while calls.load(Ordering::Relaxed) == 0 && Instant::now() < grace_deadline {
    std::thread::yield_now();
  }
  assert!(
    calls.load(Ordering::Relaxed) > 0,
    "wake callback should be invoked after successful send"
  );

  drop(ui_tx);
  drop(ui_rx);
  join.join().expect("join worker");
}

#[test]
fn ui_worker_wake_callback_not_invoked_after_receiver_drop() {
  let _lock = super::stage_listener_test_lock();

  let calls = Arc::new(AtomicUsize::new(0));
  let wake: WorkerWakeCallback = {
    let calls = Arc::clone(&calls);
    Arc::new(move || {
      calls.fetch_add(1, Ordering::Relaxed);
    })
  };

  let (ui_tx, ui_rx_raw, join) =
    spawn_browser_ui_worker("ui-worker-wake-callback-disconnect", Some(wake))
      .expect("spawn_browser_ui_worker");
  calls.store(0, Ordering::Relaxed);
  drop(ui_rx_raw);

  let tab_id = TabId::new();
  ui_tx
    .send(UiToWorker::SelectDropdownCancel { tab_id })
    .expect("send cancel");

  drop(ui_tx);
  join.join().expect("join worker");

  assert_eq!(
    calls.load(Ordering::Relaxed),
    0,
    "wake callback should not run after worker receiver disconnect"
  );
}
