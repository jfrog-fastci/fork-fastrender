#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker};
use fastrender::ui::worker_loop::spawn_ui_worker;
use std::time::{Duration, Instant};

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
  let handle = spawn_ui_worker("fastr-ui-worker-shutdown-drop").expect("spawn ui worker");

  let join = std::thread::spawn(move || handle.shutdown());
  join_with_timeout(join, Duration::from_secs(5)).expect("worker thread should not panic");
}

#[test]
fn dropping_ui_receiver_does_not_panic_worker() {
  let handle = spawn_ui_worker("fastr-ui-worker-shutdown-drop-ui-rx").expect("spawn ui worker");
  let (ui_tx, ui_rx, join_handle) = handle.split();

  drop(ui_rx);

  let tab_id = TabId(1);
  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
    })
    .expect("send CreateTab");
  ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url: "about:blank".to_string(),
      reason: NavigationReason::TypedUrl,
    })
    .expect("send Navigate");

  drop(ui_tx);

  let join = std::thread::spawn(move || join_handle.join());
  join_with_timeout(join, Duration::from_secs(15)).expect("worker thread should not panic");
}
