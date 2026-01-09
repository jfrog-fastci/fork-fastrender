//! Backwards-compatible re-export path for the canonical UI↔worker loop.
//!
//! The single implementation of the browser UI worker protocol lives in
//! [`crate::ui::render_worker`]. This module exists so older tests/tools (and downstream code) can
//! import worker entrypoints from a stable `ui::worker_loop` namespace without taking a dependency
//! on the internal module layout.

pub use super::render_worker::{
  spawn_browser_ui_worker, spawn_browser_worker, spawn_browser_worker_with_factory,
  spawn_browser_worker_with_name, spawn_ui_worker, spawn_ui_worker_for_test,
  spawn_ui_worker_with_factory, BrowserWorkerHandle, UiWorkerHandle,
};

#[cfg(any(test, feature = "browser_ui"))]
pub use super::render_worker::spawn_browser_worker_for_test;

