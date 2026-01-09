#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::cancel::CancelGens;
use fastrender::render_control::StageHeartbeat;
use fastrender::ui::messages::{TabId, WorkerToUi};
use std::ffi::OsString;
use std::time::{Duration, Instant};

// Allow plenty of time for the heavy page to render under CI contention.
//
// `about:test-heavy` intentionally does significant layout/paint work, and the UI worker runs in
// debug mode for these integration tests. Some CI environments can take >10s to reach the paint
// stages, so keep this generous to avoid flakes while still bounding the test.
const TIMEOUT: Duration = Duration::from_secs(30);

struct EnvVarGuard {
  key: &'static str,
  previous: Option<OsString>,
}

impl EnvVarGuard {
  fn set(key: &'static str, value: &str) -> Self {
    let previous = std::env::var_os(key);
    std::env::set_var(key, value);
    Self { key, previous }
  }
}

impl Drop for EnvVarGuard {
  fn drop(&mut self) {
    match self.previous.take() {
      Some(value) => std::env::set_var(self.key, value),
      None => std::env::remove_var(self.key),
    }
  }
}

#[test]
fn paint_cancellation_during_navigation_does_not_surface_error_page() {
  let _lock = super::stage_listener_test_lock();
  // Keep renders fast enough to complete within CI timeouts. This test relies on the heavy DOM to
  // keep paints in-flight long enough for cancellation.
  let _env = EnvVarGuard::set("FASTR_TEST_RENDER_DELAY_MS", "0");

  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");

  let tab_id = TabId::new();
  let cancel = CancelGens::new();

  worker
    .tx
    .send(support::create_tab_msg_with_cancel(
      tab_id,
      Some("about:test-heavy".to_string()),
      cancel.clone(),
    ))
    .expect("CreateTab");
  worker
    .tx
    .send(support::viewport_changed_msg(tab_id, (128, 128), 1.0))
    .expect("ViewportChanged");

  let mut saw_nav_committed = false;
  let mut saw_frame = false;
  let mut triggered_paint_cancel = false;
  let mut msgs: Vec<WorkerToUi> = Vec::new();
  let deadline = Instant::now() + TIMEOUT;

  while Instant::now() < deadline {
    match worker.rx.recv_timeout(Duration::from_millis(200)) {
      Ok(msg) => {
        msgs.push(msg);
        match msgs.last().expect("msg just pushed") {
          WorkerToUi::Stage { tab_id: got, stage } if *got == tab_id => {
            // Cancel the in-flight paint attempt as soon as we observe the paint stages begin.
            // This ensures we exercise the worker's "paint cancelled" error-handling path.
            if !triggered_paint_cancel
              && matches!(
                stage,
                StageHeartbeat::PaintBuild | StageHeartbeat::PaintRasterize
              )
            {
              cancel.bump_paint();
              triggered_paint_cancel = true;
            }
          }
          WorkerToUi::DebugLog { .. } => {
            // Keep debug logs (they are useful when investigating flakes) but ignore them for
            // assertions.
          }
          WorkerToUi::NavigationCommitted { tab_id: got, url, .. } if *got == tab_id => {
            assert_eq!(url, "about:test-heavy");
            saw_nav_committed = true;
          }
          WorkerToUi::NavigationFailed { tab_id: got, url, error } if *got == tab_id => {
            panic!("navigation failed unexpectedly for {url}: {error}");
          }
          WorkerToUi::FrameReady { tab_id: got, .. } if *got == tab_id => {
            saw_frame = true;
            if saw_nav_committed {
              break;
            }
          }
          _ => {}
        }
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  assert!(
    triggered_paint_cancel,
    "expected to observe paint stage and bump CancelGens; got:\n{}",
    support::format_messages(&msgs)
  );
  assert!(
    saw_nav_committed,
    "expected NavigationCommitted; got:\n{}",
    support::format_messages(&msgs)
  );
  assert!(
    saw_frame,
    "expected FrameReady after paint cancellation window; got:\n{}",
    support::format_messages(&msgs)
  );

  drop(worker.tx);
  worker.join.join().expect("worker join");
}
