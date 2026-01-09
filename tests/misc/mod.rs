//! Miscellaneous tests consolidated from tests/*.rs
 
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Serialises tests that mutate process-wide state (environment variables, stage listeners, etc).
///
/// Historically, many of these tests lived in their own `tests/*.rs` integration binaries, so they
/// ran in isolated processes. After consolidating into harnesses, they now share global state and
/// must coordinate to remain deterministic.
pub(crate) fn global_test_lock() -> MutexGuard<'static, ()> {
  static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
  LOCK
    .get_or_init(|| Mutex::new(()))
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
}

mod audio_without_controls_hidden;
mod background_clip_text_parallel;
mod base_url_dom_override;
mod bench_limits_env_test;
mod bot_mitigation_fetch_errors;
mod canvas_transparent;
mod caption_default_text_align;
mod color_glyph_opacity;
mod composed_dom_snapshot_test;
mod containment_intrinsic_inline_size_test;
mod data_url_svg;
mod datalist_hidden;
mod debug_info_semantics_guard;
mod debug_snapshot_tests;
mod dl_default_margins;
mod docs_conformance_presence;
mod docs_presence;
mod embed_object_html_renders_document;
mod error_format_snapshot;
mod external_stylesheet_integration;
mod fetch_and_render_exit_presence;
mod figure_default_margins;
mod fixed_position_ignores_viewport_scroll_test;
mod form_control_placeholder_opacity;
mod form_control_placeholder_whitespace_semantics;
mod fragmentation_columns_public_api;
mod fragmentation_public_api;
mod has_argument_validation_test;
mod inspect_api;
mod integration_test;
mod is_alias_matches_test;
mod dom2_closest;
mod dom2_js_events;
mod js_current_script;
mod js_event_loop_timers;
mod js_diagnostics;
mod js_dom_shims;
mod js_dom_exception;
mod js_fetch_bindings;
mod js_timers;
mod js_trace_spans_test;
mod js_time_determinism;
mod vm_js_define_own_property_smoke;
mod vm_js_function_call_apply_bind_smoke;
mod vm_js_module_graph_loader_smoke;
mod vm_js_object_builtins_smoke;
mod vm_js_optional_chaining_this;
mod vm_js_promise_smoke;
mod logical_shorthand_parsing_test;
mod map_hidden;
mod meta_viewport_test;
mod url_client_redirects;
mod no_merge_markers;
mod offset_anchor_parsing_test;
mod pages_multilingual_fixture_test;
mod part_export_map_test;
mod prepare_dom;
mod prepare_dom_mutation_test;
mod prepared_document_repaint;
mod prepared_document_web_font_isolation;
mod preserve3d_env_var_disable_warp_test;
mod rayon_global_pool_test;
mod referrer_credentials_test;
mod replaced_element_max_width_toggle;
mod root_background_extends_to_viewport;
mod session_paint;
mod stage_listener_guard_tests;
mod source_track_hidden;
mod style_regressions_presence;
mod stylesheet_referrer_policy_header_import_test;
mod taffy_perf_counters_diagnostics_reset_test;
mod template_inert_styles;
mod test_public_api;
mod textarea_runtime_value;
mod thread_safe_renderer;
mod timeline_scope_supports_test;
mod trace_output_test;
mod transition_behavior_property_test;
mod ua_form_control_defaults_test;
mod vendor_prefix_aliasing_test;
mod video_placeholder_test;
