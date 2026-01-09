//! Browser integration tests consolidated from tests/browser_*.rs

mod document;
mod document2;
mod browser_mem_limit_env;
mod support;
mod ui_render_worker_thread_builder_test;
mod ui_stage_heartbeat_forwarding;
mod ui_worker_scroll;
mod ui_worker_tabs;

// `GlobalStageListenerGuard` is process-global, so tests that use stage heartbeats must not run
// concurrently within this test binary.
#[cfg(feature = "browser_ui")]
pub(crate) fn stage_listener_test_lock() -> std::sync::MutexGuard<'static, ()> {
  static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
  LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
mod ui_worker_interaction;
