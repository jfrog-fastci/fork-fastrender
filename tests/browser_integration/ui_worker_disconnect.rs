#![cfg(feature = "browser_ui")]

use super::support::{create_tab_msg, navigate_msg, viewport_changed_msg, DEFAULT_TIMEOUT};
use super::worker_harness::{format_events, WorkerHarness, WorkerToUiEvent};
use fastrender::ui::messages::{NavigationReason, TabId};

#[test]
fn worker_harness_wait_for_disconnect_observes_worker_panic() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();

  let h = WorkerHarness::spawn();
  let tab_id = TabId::new();

  // Ensure the worker thread is up and producing frames before triggering the crash.
  h.send(create_tab_msg(tab_id, Some("about:newtab".to_string())));
  h.send(viewport_changed_msg(tab_id, (200, 120), 1.0));
  let _ = h.wait_for_frame(tab_id, DEFAULT_TIMEOUT);
  let pre_crash_log = h.drain_for(std::time::Duration::from_millis(100));

  // Trigger a deterministic worker panic.
  h.send(navigate_msg(
    tab_id,
    "crash://panic".to_string(),
    NavigationReason::TypedUrl,
  ));

  let events = h.assert_disconnect_within(DEFAULT_TIMEOUT);
  assert!(
    events.iter().any(|ev| matches!(
      ev,
      WorkerToUiEvent::DebugLog { line, .. } if line.contains("crash://panic")
    )),
    "expected crash debug log event.\npre-crash drain:\n{pre_crash_log}\nrecent events:\n{}",
    format_events(&events)
  );
}
