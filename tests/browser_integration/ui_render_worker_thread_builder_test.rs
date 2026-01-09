#![cfg(feature = "browser_ui")]

use fastrender::ui::spawn_browser_worker;

#[test]
fn browser_render_worker_thread_is_spawned_via_thread_builder() {
  // We can't reliably trigger a stack overflow in CI, but we can at least assert that the browser
  // UI render worker thread is created via `std::thread::Builder` (naming requires it), and the
  // implementation sets a large stack size.
  let worker = spawn_browser_worker().expect("spawn browser worker");
  let name = worker
    .join
    .thread()
    .name()
    .expect("browser worker thread should be named");
  assert_eq!(name, "browser_worker");

  let fastrender::ui::BrowserWorkerHandle { tx, join, .. } = worker;
  drop(tx);
  join.join().expect("join browser worker thread");
}
