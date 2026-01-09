#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::WorkerToUi;
use fastrender::ui::worker::spawn_render_worker_thread;
use fastrender::FastRender;

#[test]
fn browser_render_worker_thread_is_spawned_via_thread_builder() {
  let _lock = super::stage_listener_test_lock();
  // We can't reliably trigger a stack overflow in CI, but we can at least assert that the browser
  // UI render worker thread is created via `std::thread::Builder` (naming requires it), and the
  // implementation sets a large stack size.
  let renderer = FastRender::builder().build().expect("renderer");
  let (tx, _rx) = std::sync::mpsc::channel::<WorkerToUi>();

  let expected_name = "fastr-browser-render-worker-test";
  let handle = spawn_render_worker_thread(expected_name, renderer, tx, |_worker| {
    std::thread::current()
      .name()
      .expect("render worker thread should be named")
      .to_string()
  })
  .expect("spawn render worker thread");

  let observed = handle.join().expect("join render worker thread");
  assert_eq!(observed, expected_name);
}
