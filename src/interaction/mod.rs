pub mod anchor_scroll;
pub mod dom_index;
pub mod dom_mutation;
pub mod engine;
pub mod focus_scroll;
pub mod form_submit;
pub mod fragment_geometry;
pub mod hit_test;
pub mod hit_testing;
mod image_maps;
pub mod scroll_wheel;
pub mod url;

pub use anchor_scroll::scroll_offset_for_fragment_target;
pub use engine::{InputModality, InteractionAction, InteractionEngine, KeyAction};
pub use form_submit::{
  form_submission, form_submission_get_url, FormSubmission, FormSubmissionEnctype, FormSubmissionMethod,
};
pub use fragment_geometry::absolute_bounds_for_box_id;
pub use fragment_geometry::content_rect_for_border_rect;
pub use hit_test::{hit_test_dom, resolve_label_associated_control, HitTestKind, HitTestResult};
pub use hit_testing::{
  fragment_tree_with_scroll, hit_test_dom_viewport_point, hit_test_dom_with_scroll, hit_test_with_scroll,
};
pub use url::resolve_url;

use crate::style::ComputedStyle;
use crate::text::font_db::{FontConfig, FontStretch, FontStyle as DbFontStyle, ScaledMetrics};
use crate::text::font_loader::FontContext;
use std::sync::OnceLock;

/// Resolve the scaled font metrics for an element style.
///
/// This is primarily used by UI interaction code that needs to mirror paint-time sizing decisions
/// (e.g. listbox `<select>` row heights) without direct access to the renderer's `FontContext`.
///
/// Note: When the UI is built with `browser_ui`, it uses bundled fonts for deterministic output;
/// match that here so hit-testing aligns with what the painter rendered.
pub(crate) fn resolve_scaled_metrics_for_interaction(style: &ComputedStyle) -> Option<ScaledMetrics> {
  static FONT_CTX: OnceLock<FontContext> = OnceLock::new();
  let font_ctx = FONT_CTX.get_or_init(|| {
    #[cfg(feature = "browser_ui")]
    let config = FontConfig::bundled_only();
    #[cfg(not(feature = "browser_ui"))]
    let config = FontConfig::default();
    FontContext::with_config(config)
  });

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
