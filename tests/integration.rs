//! Unified integration test binary.
//!
//! Cargo treats each `tests/*.rs` file as its own integration test crate. This crate pulls the
//! integration test module trees under `tests/` into a single binary so the suite links once.

mod common;
mod api;
mod accessibility;
mod animation;
mod interaction;
mod animation;
mod fixtures;
mod browser_integration;
mod layout;
mod paint;
mod backdrop;
mod legacy;
mod css_integration;
mod determinism;
mod dom_integration;
mod iframe;
mod image_integration;
mod js_harness;
mod grid;
mod display_list;
mod misc;
mod progress;
mod render;
mod scroll;
mod tree;
mod ui;
mod guards;
mod js;
mod resource;
mod wpt;

// Keep the reference image comparison helpers available for fixture-style tests.
#[allow(dead_code)]
mod r#ref;
mod tooling;

// Regression tests under `tests/regression/`.
mod regression;
mod bin;
mod bundled;

#[test]
fn fuzz_corpus_smoke_test() {
  tooling::fuzz_corpus_smoke::fuzz_corpus_smoke_test();
}
