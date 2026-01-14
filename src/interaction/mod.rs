pub mod anchor_scroll;
pub mod anchor_geometry;
pub mod autofocus;
pub mod autofocus_dom2;
pub mod cursor;
pub mod document_selection;
pub mod dom_geometry;
pub mod dom_index;
pub mod dom_mutation;
pub mod dom_mutation_dom2;
pub(crate) mod effective_disabled;
pub(crate) mod effective_disabled_dom2;
pub mod element_geometry;
pub mod engine;
pub mod engine_dom2;
pub mod focus_scroll;
pub(crate) mod form_controls;
pub mod form_submit;
pub mod fragment_geometry;
pub mod hit_test;
pub mod hit_testing;
pub(crate) mod label_assoc_dom2;
mod image_maps;
pub(crate) mod paint_overlays;
pub mod scroll_wheel;
pub(crate) mod textarea_scroll;
pub mod selection_serialize;
pub mod state;
pub(crate) mod textarea_caret_scroll;
pub mod url;

#[cfg(test)]
mod state_dom2_tests;

pub use anchor_scroll::scroll_offset_for_fragment_target;
pub use anchor_geometry::styled_node_anchor_css;
pub use cursor::cursor_kind_for_hit;
pub use engine::{
  DateTimeInputKind, DragDropKind, InputModality, InteractionAction, InteractionEngine, KeyAction,
};
pub use engine_dom2::InteractionEngineDom2;
pub use element_geometry::{element_geometry_for_styled_node_id, ElementBoxGeometry};
pub use form_submit::{
  form_submission, form_submission_dom2, form_submission_from_submitter_dom2, form_submission_get_url,
  form_submission_get_url_dom2, form_submission_get_url_from_submitter_dom2,
  form_submission_without_submitter_dom2, Dom2FileInputLookup, FormSubmission, FormSubmissionEnctype,
  FormSubmissionMethod,
};
pub use fragment_geometry::absolute_bounds_for_box_id;
pub use fragment_geometry::absolute_bounds_by_styled_node_id;
pub use fragment_geometry::content_rect_for_border_rect;
pub use fragment_geometry::padding_rect_for_border_rect;
pub use fragment_geometry::scrollbar_reservation_for_box_id;
pub use fragment_geometry::scrollport_rect_for_padding_rect;
pub use hit_test::{
  hit_test_dom, resolve_label_associated_control, HitTestContext, HitTestKind, HitTestResult,
};
pub use hit_testing::{
  fragment_tree_with_scroll, fragment_tree_with_scroll_and_sticky, hit_test_dom_viewport_point,
  hit_test_dom_with_scroll, hit_test_with_scroll,
};
pub use state::{
  FormStateDom2, ImePreeditState, ImePreeditStateDom2, InteractionState, InteractionStateDom2,
  TextEditPaintStateDom2,
};
pub use url::resolve_url;

use crate::style::ComputedStyle;
use crate::text::font_db::{FontConfig, FontStretch, FontStyle as DbFontStyle, ScaledMetrics};
use crate::text::font_loader::FontContext;
use crate::text::pipeline::ShapingPipeline;
use std::sync::OnceLock;

fn interaction_font_ctx() -> &'static FontContext {
  static FONT_CTX: OnceLock<FontContext> = OnceLock::new();
  FONT_CTX.get_or_init(|| {
    // Keep interaction-time font metrics aligned with how browser UI documents are painted.
    //
    // - `browser_ui` builds render with bundled fonts for deterministic output and to avoid relying
    //   on host-installed font databases.
    // - Unit tests should also use bundled fonts so hit-testing/caret placement does not depend on
    //   external IO (system font scans) and stays deterministic across environments.
    #[cfg(any(test, feature = "browser_ui"))]
    let config = FontConfig::bundled_only();
    #[cfg(not(any(test, feature = "browser_ui")))]
    let config = FontConfig::default();
    FontContext::with_config(config)
  })
}

/// Resolve the scaled font metrics for an element style.
///
/// This is primarily used by UI interaction code that needs to mirror paint-time sizing decisions
/// (e.g. listbox `<select>` row heights) without direct access to the renderer's `FontContext`.
///
/// Note: When the UI is built with `browser_ui`, it uses bundled fonts for deterministic output;
/// match that here so hit-testing aligns with what the painter rendered.
pub(crate) fn resolve_scaled_metrics_for_interaction(
  style: &ComputedStyle,
) -> Option<ScaledMetrics> {
  let font_ctx = interaction_font_ctx();

  let italic = matches!(style.font_style, crate::style::types::FontStyle::Italic);
  let oblique = matches!(style.font_style, crate::style::types::FontStyle::Oblique(_));
  let stretch = FontStretch::from_percentage(style.font_stretch.to_percentage());
  let preferred_aspect = crate::text::pipeline::preferred_font_aspect(style, font_ctx);

  font_ctx
    .get_font_full(
      &style.font_family,
      style.font_weight.to_u16(),
      if italic {
        DbFontStyle::Italic
      } else if oblique {
        DbFontStyle::Oblique
      } else {
        DbFontStyle::Normal
      },
      stretch,
    )
    .or_else(|| font_ctx.get_sans_serif())
    .and_then(|font| {
      let used_font_size =
        crate::text::pipeline::compute_adjusted_font_size(style, &font, preferred_aspect);
      let authored = crate::text::variations::authored_variations_from_style(style);
      let variations = crate::text::face_cache::with_face(&font, |face| {
        crate::text::variations::collect_variations_for_face(
          face,
          style,
          &font,
          used_font_size,
          &authored,
        )
      })
      .unwrap_or_else(|| authored.clone());
      font_ctx.get_scaled_metrics_with_variations(&font, used_font_size, &variations)
    })
}

/// Shared `FontContext` for UI interaction code that needs deterministic font metrics.
///
/// This mirrors the `FontContext` choice in [`resolve_scaled_metrics_for_interaction`]. Keeping a
/// single accessor helps other interaction subsystems (e.g. text cursor positioning) reuse the same
/// font configuration without each creating their own `FontContext`.
pub(crate) fn font_context_for_interaction() -> &'static FontContext {
  interaction_font_ctx()
}

/// Shared `ShapingPipeline` for UI interaction code.
///
/// This avoids allocating a fresh shaping cache for every cursor-positioning query (e.g. click-to-place
/// caret mapping inside `<input>`/`<textarea>`). The pipeline internally synchronizes its caches, so a
/// process-global instance is sufficient.
pub(crate) fn shaping_pipeline_for_interaction() -> &'static ShapingPipeline {
  static SHAPER: OnceLock<ShapingPipeline> = OnceLock::new();
  SHAPER.get_or_init(ShapingPipeline::new)
}
