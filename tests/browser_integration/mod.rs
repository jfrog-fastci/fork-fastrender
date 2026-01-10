//! Browser integration tests consolidated from tests/browser_*.rs
 
// -----------------------------------------------------------------------------
// Test process initialization
// -----------------------------------------------------------------------------
//
// Many integration tests create `FastRender` instances (directly or indirectly via browser worker
// runtimes). On minimal/agent hosts, scanning system fonts can be very slow and can cause per-test
// timeouts and worker thread shutdown hangs.
//
// Prefer deterministic bundled fonts for the entire integration test process unless the caller
// explicitly opted out by setting `FASTR_USE_BUNDLED_FONTS=0`.
//
// We want this to run before *any* test executes, so use a small cross-platform "init array"
// constructor rather than relying on a particular test calling a helper first.
#[used]
#[cfg_attr(
  any(target_os = "linux", target_os = "android", target_os = "freebsd"),
  link_section = ".init_array"
)]
#[cfg_attr(any(target_os = "macos", target_os = "ios"), link_section = "__DATA,__mod_init_func")]
#[cfg_attr(target_os = "windows", link_section = ".CRT$XCU")]
static INIT_BROWSER_INTEGRATION_ENV: extern "C" fn() = {
  extern "C" fn init() {
    // The browser integration tests share global resources (stage listener, runtime toggles, font
    // caches) and can spawn multiple worker threads per test. Running them with the default
    // `cargo test` parallelism can cause lock contention and flakes (especially in CI).
    //
    // Default this integration-test binary to single-threaded execution unless the caller
    // explicitly opted into a different setting via `-- --test-threads` or `RUST_TEST_THREADS`.
    if std::env::var_os("RUST_TEST_THREADS").is_none() {
      std::env::set_var("RUST_TEST_THREADS", "1");
    }

    // Respect an explicit opt-out (e.g. FASTR_USE_BUNDLED_FONTS=0).
    if let Some(raw) = std::env::var_os("FASTR_USE_BUNDLED_FONTS") {
      if raw == "0" || raw.eq_ignore_ascii_case("false") {
        return;
      }
    }
    std::env::set_var("FASTR_USE_BUNDLED_FONTS", "1");
  }
  init
};

mod browser_headless_smoke_test;
mod browser_cli_help;
mod browser_cli_start_url_scheme;
mod browser_mem_limit_env;
mod browser_thread_base_url_across_navigations;
mod browser_worker_cancel_gens;
mod browser_worker_cancellation;
mod browser_worker_fragment_navigation;
mod browser_worker_percent_encoded_fragment;
mod browser_worker_target_pseudoclass;
mod browser_worker_thread;
mod browser_thread_cancellation;
mod browser_thread_history_scroll_restore;
mod browser_thread_select_dropdown;
mod browser_thread_select_dropdown_choose;
mod browser_thread_paint_cancellation;
mod document;
mod document2;
mod history_navigation;
mod select_dropdown;
mod select_dropdown_hidden_option_arrow_key;
mod select_listbox_hidden_option_click;
mod select_dropdown_pick;
mod select_listbox_click_scrolled;
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
mod ui_cancellation;
mod ui_select_listbox_click_after_wheel_scroll;
mod ui_render_worker_thread_builder_test;
mod ui_select_dropdown_choose;
mod ui_select_listbox_click;
mod ui_stage_heartbeat_forwarding;
mod ui_worker_base_url_isolation;
mod ui_worker_cancellation;
mod ui_worker_dpr;
mod ui_worker_zoom;
mod ui_worker_fragment_navigation;
mod ui_worker_history;
mod ui_worker_hover_active;
mod ui_worker_interaction;
mod ui_worker_form_submit;
mod ui_worker_keyboard;
mod ui_worker_navigation_errors;
mod ui_worker_navigation_messages;
mod ui_worker_robustness;
mod ui_worker_scroll;
mod ui_worker_scroll_hit_test;
mod ui_worker_anchor_scroll;
mod ui_scrolling;
mod ui_worker_stage_listener_scoping;
mod ui_worker_anchor_scroll_percent_encoded;
mod ui_worker_anchor_scroll_percent_escaped_percent;
mod ui_worker_shutdown;
mod ui_worker_tab_resource_isolation;
mod ui_worker_renderer_reuse;
mod ui_worker_tabs;
mod ui_worker_title;
mod ui_worker_about_pages;
mod ui_worker_viewport_changed;
mod worker_harness;
mod browser_thread_worker;
mod ui_worker_protocol_smoke;
mod ui_worker_unsupported_scheme;

// -----------------------------------------------------------------------------
// Global integration test environment
// -----------------------------------------------------------------------------

/// Ensure browser integration tests run with deterministic bundled fonts.
///
/// Many UI/worker tests spawn `FastRenderFactory` instances. `FontConfig::default` prefers system
/// fonts unless `FASTR_USE_BUNDLED_FONTS`/`CI` is set, which can make first-render latency highly
/// dependent on the host's installed fonts and lead to flaky timeouts. We set the bundled-fonts
/// knob once for the whole test process.
#[cfg(feature = "browser_ui")]
fn ensure_browser_test_env() {
  static INIT: std::sync::Once = std::sync::Once::new();
  INIT.call_once(|| {
    std::env::set_var("FASTR_USE_BUNDLED_FONTS", "1");
  });
}

// Browser UI integration tests occasionally rely on process-global knobs (e.g. test render delays)
// and other shared state. Serialize tests with this lock to avoid cross-test interference and keep
// CI runs deterministic under `cargo test`'s default parallelism.
#[cfg(feature = "browser_ui")]
pub(crate) fn stage_listener_test_lock() -> std::sync::MutexGuard<'static, ()> {
  ensure_browser_test_env();
  // Pre-warm bundled font metadata so the first navigation in a freshly spawned UI worker does not
  // block on expensive font parsing while the test is waiting on UI messages.
  support::ensure_bundled_fonts_loaded();
  static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
  LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
