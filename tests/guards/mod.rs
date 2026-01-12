//! Guard tests that enforce repository-wide invariants.
//!
//! These are typically "policy" or "post-migration" checks (workspace layout, dependency choices,
//! etc.) rather than behavioural renderer tests.

mod ecma_rs_workspace_exclude_guard;
mod js_runtime_consolidation_guard;
mod webidl_consolidation_guard;
mod style_regressions_presence;
