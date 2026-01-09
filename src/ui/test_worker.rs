//! Test-only helpers for spawning a headless browser UI worker.
//!
//! The production worker implementation lives in [`crate::ui::render_worker`]. This module exists
//! for backwards-compat with older integration tests that referenced `ui::test_worker`, while
//! ensuring the actual worker loop does not diverge from production.

pub use crate::ui::render_worker::{
  spawn_ui_worker, spawn_ui_worker_for_test, spawn_ui_worker_with_factory, UiWorkerHandle,
};
