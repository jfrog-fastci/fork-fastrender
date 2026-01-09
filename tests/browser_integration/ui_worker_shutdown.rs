#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{NavigationReason, TabId};
use fastrender::ui::worker_loop::spawn_ui_worker;
use std::time::{Duration, Instant};

use super::support::{create_tab_msg, navigate_msg};

fn join_with_timeout(
  handle: std::thread::JoinHandle<std::thread::Result<()>>,
  timeout: Duration,
) -> std::thread::Result<()> {
  let deadline = Instant::now() + timeout;
  while Instant::now() < deadline {
    if handle.is_finished() {
      return handle.join().expect("join helper thread");
    }
    std::thread::sleep(Duration::from_millis(5));
  }
  panic!("timed out waiting for worker join");
}

#[test]
fn dropping_handle_shuts_down_worker_thread() {
  let _lock = super::stage_listener_test_lock();
  let handle =
    spawn_ui_worker("fastr-ui-worker-shutdown-drop").expect("spawn ui worker");

  let join = std::thread::spawn(move || handle.shutdown());
  // Shutting down can involve joining render threads; allow some slack under parallel load.
  join_with_timeout(join, Duration::from_secs(10)).expect("worker thread should not panic");
}

#[test]
fn dropping_ui_receiver_does_not_panic_worker() {
  let _lock = super::stage_listener_test_lock();
  let handle = spawn_ui_worker("fastr-ui-worker-shutdown-drop-ui-rx")
    .expect("spawn ui worker");
  let (ui_tx, ui_rx, join_handle) = handle.split();

  drop(ui_rx);

  let tab_id = TabId(1);
  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("send CreateTab");
  ui_tx
    .send(navigate_msg(
      tab_id,
      "about:blank".to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("send Navigate");

  drop(ui_tx);

  let join = std::thread::spawn(move || join_handle.join());
  // Navigation work is synchronous inside the worker thread, and this test binary runs many render
  // jobs in parallel by default. Allow extra time for the worker to finish its last navigation and
  // observe channel shutdown under CPU contention.
  join_with_timeout(join, Duration::from_secs(20)).expect("worker thread should not panic");
}
