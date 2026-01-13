//! Unified integration test binary.
//!
//! Cargo treats each `tests/*.rs` file as its own integration test crate. This crate pulls the
//! integration test module trees under `tests/` into a single binary so the suite links once.

mod common;
mod accessibility;
mod animation;
mod api;
#[cfg(feature = "browser_ui")]
mod browser_integration;
mod determinism;
mod dom_integration;
mod interaction;
mod fixtures;
mod grid;
mod guards;
mod iframe;
mod image_integration;
mod video_integration;
mod js;
mod layout;
mod media;
mod misc;
mod progress;
mod render;
mod renderer_chrome;
mod resource;
mod multiprocess;
mod sandbox;
mod scroll;
mod security;
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
