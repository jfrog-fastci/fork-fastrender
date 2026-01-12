//! Guard tests that enforce repository-wide invariants.
//!
//! These are typically "policy" or "post-migration" checks (workspace layout, dependency choices,
//! etc.) rather than behavioural renderer tests.

mod browser_stub_feature_gate;
mod debug_info_semantics_guard;
mod docs_conformance_presence;
mod docs_presence;
mod ecma_rs_workspace_exclude_guard;
mod fetch_and_render_exit_presence;
mod js_runtime_consolidation_guard;
mod no_merge_markers;
mod no_production_panics;
mod style_regressions_presence;
mod webidl_consolidation_guard;
