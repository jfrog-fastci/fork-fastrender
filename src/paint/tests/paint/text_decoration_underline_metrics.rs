use std::sync::Arc;

use crate::paint::display_list::DisplayItem;
use crate::paint::display_list_builder::DisplayListBuilder;
use crate::style::types::{TextDecorationLine, TextDecorationThickness};
use crate::{ComputedStyle, FontConfig, FontContext, FragmentNode, FragmentTree, Rect, Rgba};

#[test]
fn underline_center_uses_font_metrics_without_double_counting_thickness() {
  // Make Rayon initialization deterministic; DisplayListBuilder and font shaping can hit the global
  // pool in some configs.
  crate::testing::init_rayon_for_tests(2);

  let mut style = ComputedStyle::default();
  style.color = Rgba::BLACK;
  style.font_size = 20.0;
  style.text_decoration.lines = TextDecorationLine::UNDERLINE;
  style.text_decoration.color = Some(Rgba::BLACK);
  // Keep thickness tied to the font metrics so we can validate underline position computation
  // without additional UA-defined thickness heuristics.
  style.text_decoration.thickness = TextDecorationThickness::FromFont;
  let style = Arc::new(style);

  let font_ctx = FontContext::with_config(FontConfig::bundled_only());
  let shaper = crate::text::ShapingPipeline::new();
  let runs = shaper.shape("Hi", &style, &font_ctx).expect("shape");
  let first_run = runs.first().expect("expected at least one run");

  let scaled = font_ctx
    .get_scaled_metrics_with_variations(
      first_run.font.as_ref(),
      first_run.font_size,
      &first_run.variations,
    )
    .expect("scaled metrics");

  // Fragment baseline is rect.y + baseline_offset for horizontal text.
  let baseline_offset = 10.0;
  let baseline_y = baseline_offset;
  let expected_center = baseline_y - scaled.underline_position;

  let text_rect = Rect::from_xywh(0.0, 0.0, 100.0, 40.0);
  let fragment = FragmentNode::new_text_shaped(
    text_rect,
    "Hi",
    baseline_offset,
    Arc::new(runs),
    Arc::clone(&style),
  );
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 40.0), vec![fragment]);
  let tree = FragmentTree::new(root);

  let list = DisplayListBuilder::new()
    .with_font_context(font_ctx)
    .build_tree(&tree);

  let mut underline_centers = Vec::new();
  for item in list.items() {
    let DisplayItem::TextDecoration(decoration) = item else {
      continue;
    };
    for paint in &decoration.decorations {
      if let Some(stroke) = &paint.underline {
        underline_centers.push(stroke.center);
      }
    }
  }

  assert_eq!(
    underline_centers.len(),
    1,
    "expected exactly one underline stroke, got {underline_centers:?}"
  );
  let center = underline_centers[0];
  assert!(
    (center - expected_center).abs() < 0.01,
    "underline center expected {expected_center}, got {center} (font underline_position={} underline_thickness={})",
    scaled.underline_position,
    scaled.underline_thickness
  );
}
