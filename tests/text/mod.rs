//! Text/shaping integration tests.

use std::sync::{Mutex, OnceLock};

fn text_diagnostics_mutex() -> &'static Mutex<()> {
  static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
  LOCK.get_or_init(|| Mutex::new(()))
}

/// Text diagnostics are collected via process-global state, so tests that enable diagnostics must
/// not overlap with tests that deliberately trigger last-resort fallback.
pub(super) fn text_diagnostics_guard() -> std::sync::MutexGuard<'static, ()> {
  text_diagnostics_mutex()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
}

mod bidi;
mod bidi_visual_order;
mod bundled_emoji_last_resort;
mod bundled_emoji_render;
mod bundled_multiscript_render;
mod bundled_script_coverage;
mod cluster_test;
mod color_font_detection_test;
mod emoji_font_detection_test;
mod emoji_font_finder_test;
mod emoji_test;
mod emoji_unified_test;
mod font_face_typography_descriptors;
mod font_fallback;
mod font_metrics_test;
mod font_palette;
mod font_palette_test;
mod font_size_adjust_metrics;
mod font_variant_position_synthesis_test;
mod native_small_caps_feature_probe_test;
mod font_variant_emoji_monochrome_test;
mod font_variation_backend;
mod generic_family_mapping_test;
mod hyphenation_test;
mod justify_test;
mod letter_spacing_ligatures_test;
mod line_break_test;
mod pipeline_test;
mod script_test;
mod shaper_test;
mod svg_color_font_test;
mod svg_glyph_safety_test;
mod variable_font_metrics_mvar;
mod web_font_cors;
mod web_font_display;
