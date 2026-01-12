//! Miscellaneous tests consolidated from tests/*.rs

/// Serialises tests that mutate process-wide state (environment variables, current dir, global
/// stage listeners, etc).
///
/// The implementation lives in `tests/common/` so the consolidated `tests/integration.rs` harness
/// can share a single lock across modules.
pub(crate) fn global_test_lock() -> crate::common::GlobalTestLockGuard {
  crate::common::global_test_lock()
}
mod background_clip_text_parallel;
mod color_glyph_opacity;
mod composed_dom_snapshot_test;
mod containment_intrinsic_inline_size_test;
mod data_url_svg;
mod debug_snapshot_tests;
mod dom2_closest;
mod error_format_snapshot;
mod fixed_position_ignores_viewport_scroll_test;
mod has_argument_validation_test;
mod inspect_api;
mod is_alias_matches_test;
mod js_dom_exception;
mod js_dom_realm_selectors;
mod js_css_supports;
mod js_dom_wrapper_identity;
mod js_execution_budgets;
mod js_intersection_observer;
mod js_time_determinism;
mod js_url_bindings;
mod js_vm_budget_tests;
mod js_webidl_binding_value_array_return;
mod js_webidl_sequence_conversion;
mod js_webidl_window_alert;
mod js_window_host_base_url_update;
mod js_window_realm;
mod logical_shorthand_parsing_test;
mod offset_anchor_parsing_test;
mod part_export_map_test;
mod preserve3d_env_var_disable_warp_test;
mod rayon_global_pool_test;
mod referrer_credentials_test;
mod replaced_element_max_width_toggle;
mod template_inert_styles;
mod textarea_runtime_value;
mod timeline_scope_supports_test;
mod transition_behavior_property_test;
mod ua_form_control_defaults_test;
mod vendor_prefix_aliasing_test;
mod vm_js_crypto_subtle_digest;
mod vm_js_define_own_property_smoke;
mod vm_js_dom_exception_smoke;
mod vm_js_dynamic_script_crossorigin_cors;
mod vm_js_function_call_apply_bind_smoke;
mod vm_js_function_object_properties_smoke;
mod vm_js_hooks_payload_regressions;
mod vm_js_module_graph_loader_smoke;
mod vm_js_new_target_smoke;
mod vm_js_object_builtins_smoke;
mod vm_js_optional_chaining_this;
mod vm_js_promise_job_rooting;
mod vm_js_promise_smoke;
mod vm_js_webidl_constructors;
mod vm_js_window_host_import_maps_dynamic_import;
