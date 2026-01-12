//! Layout integration tests (public API only).
//!
//! These tests exercise the public `FastRender` API end-to-end.
//! Layout regression/unit tests that import internal modules live in `src/layout/tests/` so they
//! run under `cargo test --lib` and do not require a separate integration test binary.

mod absolute_position_body;
mod abspos_bottom_inset_auto_height_containing_block;
mod box_shadow_outset_cutout;
mod fixed_flex_auto_height_does_not_use_intrinsic_block_size;
mod float_external_base_x;
mod grid_column_shorthand_longhand_merge;
mod html_body_percent_height;
mod image_missing_placeholder_does_not_scale_to_width;
mod image_percent_height_missing_uses_alt_intrinsic;
mod intrinsic_memoization_stress;
mod legacy_webkit_box_flex;
mod padding_offsets;
mod paged_media;
mod parallel_stats;
mod profile_diagnostics;
mod render_wrap;
mod text_wrap_pretty_does_not_rebalance;

mod test_locks;

fn layout_parallel_debug_lock() -> parking_lot::MutexGuard<'static, ()> {
  test_locks::layout_parallel_debug_lock()
}

fn layout_profile_lock() -> parking_lot::MutexGuard<'static, ()> {
  test_locks::layout_profile_lock()
}
