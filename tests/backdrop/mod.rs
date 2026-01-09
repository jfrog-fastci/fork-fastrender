//! Backdrop filter / Backdrop Root integration tests.
//!
//! These tests are included by the `paint_tests` integration-test harness (see `tests/paint_tests.rs`).
//!
//! Historically some of these lived in their own `tests/*.rs` integration-test crates; keep them
//! consolidated under this directory to avoid spawning dozens of separate test executables.

mod backdrop_filter_clip_and_radius;
mod backdrop_filter_determinism_regression;
mod backdrop_filter_filter_interaction_test;
mod backdrop_filter_mask_image_test;
mod backdrop_filter_parallel_test;
mod backdrop_filter_through_blend_isolation;
mod backdrop_root_backdrop_filter_test;
mod backdrop_root_clip_path_test;
mod backdrop_root_demand_driven_layers_test;
mod backdrop_root_filter_and_mask_test;
mod backdrop_root_intermediate_layer_test;
mod backdrop_root_matrix_test;
mod backdrop_root_more_triggers_test;
mod backdrop_root_nested_backdrop_filter_test;
mod backdrop_root_non_trigger_stacking_contexts_test;
mod backdrop_root_non_triggers_test;
mod backdrop_root_semantics_test;
mod backdrop_root_triggers_test;
mod backdrop_root_will_change_test;
mod trace_backdrop_stack_smoke_test;
