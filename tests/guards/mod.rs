//! Guard tests that enforce repository-wide invariants.
//!
//! These are typically "policy" or "post-migration" checks (workspace layout, dependency choices,
//! etc.) rather than behavioural renderer tests.

mod webidl_consolidation_guard;
