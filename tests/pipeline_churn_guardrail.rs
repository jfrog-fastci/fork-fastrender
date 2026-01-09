//! Pipeline churn regression tests.
//!
//! These tests reset global debug counters (factory creation, detached clones, etc.). The main
//! `layout_tests` integration test binary executes hundreds of layout tests in parallel, so keeping
//! churn guardrails in the same process can introduce cross-test contention/flakiness.
//!
//! By hosting them in a dedicated integration test binary we ensure the counters only observe the
//! layout work performed by these tests.

#[path = "layout/pipeline_churn_guardrail.rs"]
mod pipeline_churn_guardrail;

