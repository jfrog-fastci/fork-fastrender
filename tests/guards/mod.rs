//! Guard tests that enforce repository-wide invariants.
//!
//! These are typically "policy" or "post-migration" checks (workspace layout, dependency choices,
//! etc.) rather than behavioural renderer tests.

mod browser_stub_feature_gate;
mod crates_directory_guard;
mod debug_info_semantics_guard;
mod docs_conformance_presence;
mod docs_presence;
mod ecma_rs_workspace_exclude_guard;
mod fetch_and_render_exit_presence;
mod js_runtime_consolidation_guard;
mod no_deprecated_test_harness_names;
mod no_path_env_mutation;
mod no_fastr_use_bundled_fonts_env_mutation;
mod no_merge_markers;
mod no_production_panics;
mod stage_listener_guard_tests;
mod style_regressions_presence;
mod test_cleanup_inventory_guard;
mod webidl_consolidation_guard;
mod webidl_stale_crates_paths_guard;
mod webidl_vm_js_workspace_guard;
