//! Miscellaneous tests consolidated from tests/*.rs

/// Serialises tests that mutate process-wide state (environment variables, current dir, global
/// stage listeners, etc).
///
/// The implementation lives in `tests/common/` so the consolidated `tests/integration.rs` harness
/// can share a single lock across modules.
pub(crate) fn global_test_lock() -> crate::common::GlobalTestLockGuard {
  crate::common::global_test_lock()
}
mod chrome_api_tests;
mod data_url_svg;
mod error_format_snapshot;
mod font_db_generic_fallbacks;
mod js_css_supports;
mod js_dom_exception;
mod js_dom_realm_selectors;
mod js_dom_wrapper_identity;
mod js_execution_budgets;
mod js_indexed_db_shim;
mod js_intersection_observer;
mod js_data_transfer_items_files;
mod js_range_stringifier;
mod js_time_determinism;
mod js_url_bindings;
mod js_vm_budget_tests;
mod js_shadow_root_mutation;
mod js_vm_top_level_await;
mod js_webidl_binding_value_array_return;
mod js_webidl_sequence_conversion;
mod js_webidl_insert_adjacent;
mod js_webidl_window_alert;
mod js_window_host_base_url_update;
mod js_window_realm;
mod preserve3d_env_var_disable_warp_test;
mod rayon_global_pool_test;
mod readable_stream_start_throw;
mod readable_stream_start_throw_symbol;
mod readable_stream_start_non_callable;
mod readable_stream_start_promise_reject;
mod readable_stream_start_promise_reject_symbol;
mod readable_stream_start_thenable_reject;
mod readable_stream_controller_error_symbol;
mod replaced_element_max_width_toggle;
mod readable_stream_desired_size;
mod streams_tee_uninitialized;
mod transform_stream_async_transform_rejection;
mod transform_stream_async_transform_rejection_symbol;
mod transform_stream_abort_symbol;
mod transform_stream_controller_error_symbol;
mod text_decoder_stream;
mod transition_behavior_property_test;
mod vm_js_crypto_subtle_digest;
mod vm_js_crypto_subtle_jwk_oct;
mod vm_js_dynamic_script_crossorigin_cors;
mod vm_js_window_host_import_maps_dynamic_import;
