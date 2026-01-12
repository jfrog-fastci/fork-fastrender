mod support;
mod additional_fragment_offsets;
mod clip_path_shape_radius_keywords;
mod element_scroll_timeline;
mod running_anchor_snapshots;
mod scroll_function_timeline;
mod timeline_scope;

// Consolidated from tests/animation_*.rs
mod clip_path_reference_box_percentages;
mod range_length_offsets_tests;
mod range_strict_tests;
mod shorthand_reset_tests;
mod time_precision_test;
mod time_sampling;
mod timeline_scope_tests;
mod timeline_supports_test;

// Consolidated from standalone transition/animation integration tests.
mod browser_document_transition_state;
mod transitions_dynamic_engine_test;
mod transitions_dynamic_value_pair_discrete_test;

// Legacy tests pulled in from the former top-level `tests/animation_tests.rs` crate.
mod animation_tests;
