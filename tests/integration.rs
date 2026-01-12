//! Unified integration test binary.
//!
//! Cargo treats each `tests/*.rs` file as its own integration test crate. This crate pulls the
//! integration test module trees under `tests/` into a single binary so the suite links once.

mod common;
mod accessibility;
mod animation;
mod api;
mod browser_integration;
mod determinism;
mod dom_integration;
mod fixtures;
mod grid;
mod guards;
mod iframe;
mod image_integration;
mod interaction;
mod js;
mod layout;
mod misc;
mod progress;
mod render;
mod resource;
mod scroll;
mod tree;
mod ui;
mod wpt;

// Keep the reference image comparison helpers available for fixture-style tests.
#[allow(dead_code)]
mod r#ref;
mod tooling;

// Regression tests under `tests/regression/`.
mod regression;
mod bin;
mod bundled;
