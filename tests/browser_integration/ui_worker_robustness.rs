#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  KeyAction, NavigationReason, PointerButton, RepaintReason, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::worker::spawn_ui_worker;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

const QUIET_WINDOW: Duration = Duration::from_millis(200);
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);

fn send_noise_messages(tx: &Sender<UiToWorker>, tab_id: TabId) {
  tx
    .send(support::viewport_changed_msg(tab_id, (64, 64), 1.0))
    .expect("send ViewportChanged");
  tx
    .send(support::scroll_msg(tab_id, (0.0, 10.0), Some((5.0, 6.0))))
    .expect("send Scroll");
  tx
    .send(support::navigate_msg(
      tab_id,
      "about:blank".to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("send Navigate");
  tx.send(UiToWorker::PointerMove {
    tab_id,
    pos_css: (5.0, 6.0),
    button: PointerButton::Primary,
  })
  .expect("send PointerMove");
  tx.send(UiToWorker::PointerDown {
    tab_id,
    pos_css: (5.0, 6.0),
    button: PointerButton::Primary,
  })
  .expect("send PointerDown");
  tx.send(UiToWorker::PointerUp {
    tab_id,
    pos_css: (5.0, 6.0),
    button: PointerButton::Primary,
  })
  .expect("send PointerUp");
  tx.send(UiToWorker::TextInput {
    tab_id,
    text: "hello".to_string(),
  })
  .expect("send TextInput");
  tx.send(UiToWorker::KeyAction {
    tab_id,
    key: KeyAction::Enter,
  })
  .expect("send KeyAction");
  tx.send(UiToWorker::RequestRepaint {
    tab_id,
    reason: RepaintReason::Explicit,
  })
  .expect("send RequestRepaint");
}

fn is_tab_effect_message(msg: &WorkerToUi, tab_id: TabId) -> bool {
  match msg {
    WorkerToUi::Stage { tab_id: msg_tab, .. }
    | WorkerToUi::FrameReady { tab_id: msg_tab, .. }
    | WorkerToUi::OpenSelectDropdown { tab_id: msg_tab, .. }
    | WorkerToUi::SelectDropdownOpened { tab_id: msg_tab, .. }
    | WorkerToUi::NavigationStarted { tab_id: msg_tab, .. }
    | WorkerToUi::NavigationCommitted { tab_id: msg_tab, .. }
    | WorkerToUi::NavigationFailed { tab_id: msg_tab, .. }
    | WorkerToUi::ScrollStateUpdated { tab_id: msg_tab, .. }
    | WorkerToUi::LoadingState { tab_id: msg_tab, .. } => *msg_tab == tab_id,
    WorkerToUi::DebugLog { .. } => false,
  }
}

fn assert_no_effect_messages_for(rx: &Receiver<WorkerToUi>, tab_id: TabId, timeout: Duration) {
  let start = Instant::now();
  let mut msgs = Vec::new();

  loop {
    let remaining = timeout.saturating_sub(start.elapsed());
    if remaining.is_zero() {
      break;
    }

    match rx.recv_timeout(remaining.min(Duration::from_millis(25))) {
      Ok(msg) => {
        let is_effect = is_tab_effect_message(&msg, tab_id);
        msgs.push(msg);
        if is_effect {
          panic!(
            "unexpected WorkerToUi update for ignored tab_id={tab_id:?}:\n{}",
            support::format_messages(&msgs)
          );
        }
      }
      Err(RecvTimeoutError::Timeout) => {}
      Err(RecvTimeoutError::Disconnected) => {
        panic!(
          "worker disconnected unexpectedly while waiting for quiet window; received:\n{}",
          support::format_messages(&msgs)
        );
      }
    }
  }
}

fn wait_for_frame_ready(rx: &Receiver<WorkerToUi>, tab_id: TabId, timeout: Duration) {
  let deadline = Instant::now() + timeout;
  loop {
    let now = Instant::now();
    if now >= deadline {
      panic!("timed out waiting for FrameReady for tab_id={tab_id:?}");
    }
    let remaining = deadline - now;
    match rx.recv_timeout(remaining) {
      Ok(WorkerToUi::FrameReady { tab_id: msg_tab, .. }) if msg_tab == tab_id => return,
      Ok(_) => {}
      Err(RecvTimeoutError::Timeout) => {}
      Err(RecvTimeoutError::Disconnected) => panic!("worker disconnected unexpectedly"),
    }
  }
}

#[test]
fn messages_for_unknown_tab_are_ignored_without_panic() {
  let _lock = super::stage_listener_test_lock();
  let handle = spawn_ui_worker("fastr-ui-worker-robustness-unknown").expect("spawn ui worker");
  let (tx, rx, join) = handle.split();

  let tab1 = TabId(1);
  tx.send(support::create_tab_msg(tab1, None))
    .expect("send CreateTab");
  tx.send(support::viewport_changed_msg(tab1, (200, 120), 1.0))
    .expect("send ViewportChanged(tab1)");
  tx.send(support::navigate_msg(
    tab1,
    "about:newtab".to_string(),
    NavigationReason::TypedUrl,
  ))
  .expect("send Navigate");
  wait_for_frame_ready(&rx, tab1, FRAME_TIMEOUT);
  let _ = support::drain_for(&rx, Duration::from_millis(50));

  let unknown_tab = TabId(9999);
  send_noise_messages(&tx, unknown_tab);
  assert_no_effect_messages_for(&rx, unknown_tab, QUIET_WINDOW);

  // Ensure the worker thread is still alive and the existing tab still repaints after ignored
  // events for another tab.
  tx.send(UiToWorker::RequestRepaint {
    tab_id: tab1,
    reason: RepaintReason::Explicit,
  })
  .expect("send RequestRepaint(tab1)");
  wait_for_frame_ready(&rx, tab1, FRAME_TIMEOUT);

  drop(tx);
  join.join().expect("join ui worker");
}

#[test]
fn messages_after_close_tab_are_noops() {
  let _lock = super::stage_listener_test_lock();
  let handle = spawn_ui_worker("fastr-ui-worker-robustness-close").expect("spawn ui worker");
  let (tx, rx, join) = handle.split();

  let tab1 = TabId(1);
  tx.send(support::create_tab_msg(tab1, None))
    .expect("send CreateTab(tab1)");
  tx.send(support::viewport_changed_msg(tab1, (200, 120), 1.0))
    .expect("send ViewportChanged(tab1)");
  tx.send(support::navigate_msg(
    tab1,
    "about:newtab".to_string(),
    NavigationReason::TypedUrl,
  ))
  .expect("send Navigate(tab1)");
  wait_for_frame_ready(&rx, tab1, FRAME_TIMEOUT);
  let _ = support::drain_for(&rx, Duration::from_millis(50));

  tx.send(UiToWorker::CloseTab { tab_id: tab1 })
    .expect("send CloseTab(tab1)");
  send_noise_messages(&tx, tab1);
  assert_no_effect_messages_for(&rx, tab1, QUIET_WINDOW);

  // Ensure the worker thread is still alive by creating a second tab.
  let tab2 = TabId(2);
  tx.send(support::create_tab_msg(tab2, None))
    .expect("send CreateTab(tab2)");
  tx.send(support::viewport_changed_msg(tab2, (200, 120), 1.0))
    .expect("send ViewportChanged(tab2)");
  tx.send(support::navigate_msg(
    tab2,
    "about:newtab".to_string(),
    NavigationReason::TypedUrl,
  ))
  .expect("send Navigate(tab2)");
  wait_for_frame_ready(&rx, tab2, FRAME_TIMEOUT);

  drop(tx);
  join.join().expect("join ui worker");
}
