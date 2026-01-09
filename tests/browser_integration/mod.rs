//! Browser integration tests consolidated from tests/browser_*.rs

mod browser_headless_smoke_test;
mod browser_mem_limit_env;
mod browser_worker_cancel_gens;
mod browser_worker_cancellation;
mod browser_worker_fragment_navigation;
mod browser_worker_target_pseudoclass;
mod browser_worker_thread;
mod browser_thread_cancellation;
mod browser_thread_select_dropdown;
mod browser_thread_paint_cancellation;
mod document;
mod document2;
mod history_navigation;
mod select_dropdown_hidden_option_arrow_key;
mod select_listbox_hidden_option_click;
mod select_dropdown_pick;
mod select_listbox_wheel_scroll;
mod js_rendering;
mod support;
mod tab;
mod ui_input_routing;
mod ui_navigation_messages;
mod ui_render_thread;
mod ui_browser_worker_live_tab;
mod ui_browser_worker_thread_naming;
mod ui_fragment_navigation;
mod ui_render_worker_thread_builder_test;
mod ui_select_dropdown_choose;
mod ui_select_listbox_click;
mod ui_stage_heartbeat_forwarding;
mod ui_worker_base_url_isolation;
mod ui_worker_cancellation;
mod ui_worker_history;
mod ui_worker_hover_active;
mod ui_worker_interaction;
mod ui_worker_form_submit;
mod ui_worker_keyboard;
mod ui_worker_fragment_navigation;
mod ui_worker_navigation_errors;
mod ui_worker_navigation_messages;
mod ui_worker_scroll;
mod ui_worker_scroll_hit_test;
mod ui_worker_anchor_scroll;
mod ui_scrolling;
mod ui_history_scroll_restore;
mod ui_worker_stage_listener_scoping;
mod ui_worker_anchor_scroll_percent_encoded;
mod ui_worker_anchor_scroll_percent_escaped_percent;
mod ui_worker_shutdown;
mod ui_worker_tab_resource_isolation;
mod ui_worker_title;
mod ui_worker_dpr;
mod ui_worker_tabs;
mod ui_worker_robustness;
mod ui_worker_viewport_changed;
mod worker_harness;
mod worker_runtime;
mod ui_worker_protocol_smoke;

// `GlobalStageListenerGuard` (used by stage heartbeat forwarding) is process-global. While it is
// installed, *all* renders in the process will invoke the listener, which can leak stage messages
// across tests and add overhead. Serialize browser UI integration tests with this lock to keep CI
// runs deterministic under `cargo test`'s default parallelism.
#[cfg(feature = "browser_ui")]
pub(crate) fn stage_listener_test_lock() -> std::sync::MutexGuard<'static, ()> {
  // Pre-warm bundled font metadata so the first navigation in a freshly spawned UI worker does not
  // block on expensive font parsing while the test is waiting on UI messages.
  support::ensure_bundled_fonts_loaded();
  static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
  LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
