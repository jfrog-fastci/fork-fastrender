mod support;
mod additional_fragment_offsets;
mod clip_path_shape_radius_keywords;
mod element_scroll_timeline;
mod running_anchor_snapshots;
mod scroll_function_timeline;
mod timeline_scope;

// Consolidated from tests/animation_*.rs
mod clip_path_reference_box_percentages;
mod time_precision_test;
mod time_sampling;
mod timeline_scope_tests;

// Consolidated from standalone transition/animation integration tests.
mod browser_document_transition_state;
mod transitions_dynamic_engine_test;
mod transitions_dynamic_value_pair_discrete_test;

// Formerly a standalone `tests/*.rs` integration-test binary; now included from `tests/integration.rs`.
mod animation_tests;
