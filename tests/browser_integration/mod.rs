//! Browser integration tests consolidated from tests/browser_*.rs

mod browser_binary_headless_smoke;
mod browser_mem_limit_env;
mod document;
mod document2;
mod select_listbox_wheel_scroll;
mod support;
mod ui_input_routing;
mod ui_render_worker_thread_builder_test;
mod ui_stage_heartbeat_forwarding;
mod ui_worker_history;
mod ui_worker_interaction;
mod ui_worker_hover_active;
mod ui_worker_navigation_errors;
mod ui_worker_navigation_messages;
mod ui_worker_scroll;
mod ui_worker_scroll_hit_test;
mod ui_worker_tabs;
mod ui_worker_shutdown;
mod ui_worker_fragment_navigation;

// `GlobalStageListenerGuard` is process-global, so tests that use stage heartbeats must not run
// concurrently within this test binary.
#[cfg(feature = "browser_ui")]
pub(crate) fn stage_listener_test_lock() -> std::sync::MutexGuard<'static, ()> {
  static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
  LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
