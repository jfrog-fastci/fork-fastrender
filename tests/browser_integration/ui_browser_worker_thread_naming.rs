#![cfg(feature = "browser_ui")]

#[test]
fn spawn_browser_ui_worker_uses_requested_thread_name() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let expected = "fastr-browser-ui-worker-test";
  let (ui_tx, _ui_rx, join) =
    fastrender::ui::spawn_browser_ui_worker(expected, None).expect("spawn_browser_ui_worker");

  assert_eq!(
    join.thread().name(),
    Some(expected),
    "browser worker thread should be named via the requested value"
  );

  drop(ui_tx);
  join.join().expect("join browser worker");
}
