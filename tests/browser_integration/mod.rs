//! Browser integration tests consolidated from tests/browser_*.rs

mod browser_binary_headless_smoke;
mod browser_headless_smoke_test;
mod browser_mem_limit_env;
mod browser_worker_fragment_navigation;
mod document;
mod document2;
mod select_listbox_wheel_scroll;
mod js_rendering;
mod support;
mod ui_input_routing;
mod ui_render_thread;
mod ui_render_worker_thread_builder_test;
mod ui_select_listbox_click;
mod ui_stage_heartbeat_forwarding;
mod ui_worker_cancellation;
mod ui_worker_history;
mod ui_worker_hover_active;
mod ui_worker_interaction;
mod ui_worker_keyboard;
mod ui_worker_fragment_navigation;
mod ui_worker_navigation_errors;
mod ui_worker_navigation_messages;
mod ui_worker_scroll;
mod ui_worker_scroll_hit_test;
mod ui_worker_tabs;
mod ui_worker_shutdown;
mod ui_worker_title;
mod ui_worker_dpr;
mod worker_harness;
mod worker_runtime;

// `GlobalStageListenerGuard` (used by stage heartbeat forwarding) is process-global. While it is
// installed, *all* renders in the process will invoke the listener, which can leak stage messages
// across tests and add overhead. Serialize browser UI integration tests with this lock to keep CI
// runs deterministic under `cargo test`'s default parallelism.
#[cfg(feature = "browser_ui")]
pub(crate) fn stage_listener_test_lock() -> std::sync::MutexGuard<'static, ()> {
  static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
  LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
