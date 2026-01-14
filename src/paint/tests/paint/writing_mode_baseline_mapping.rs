use std::sync::Arc;

use crate::paint::display_list::DisplayItem;
use crate::paint::display_list_builder::DisplayListBuilder;
use crate::style::types::{Direction, WritingMode};
use crate::text::font_db::FontConfig;
use crate::{ComputedStyle, FontContext, FragmentNode, FragmentTree, Rect};

fn build_text_display_list(
  style: Arc<ComputedStyle>,
  rect: Rect,
  baseline_offset: f32,
) -> crate::DisplayList {
  crate::testing::init_rayon_for_tests(2);

  let font_ctx = FontContext::with_config(FontConfig::bundled_only());
  let shaper = crate::text::ShapingPipeline::new();
  let runs = shaper.shape("A", &style, &font_ctx).expect("shape");

  let fragment = FragmentNode::new_text_shaped(rect, "A", baseline_offset, Arc::new(runs), style);
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 400.0, 400.0), vec![fragment]);
  let tree = FragmentTree::new(root);

  DisplayListBuilder::new()
    .with_font_context(font_ctx)
    .build_tree(&tree)
}

fn first_text_item(list: &crate::DisplayList) -> &crate::PaintTextItem {
  list
    .items()
    .iter()
    .find_map(|item| match item {
      DisplayItem::Text(text) if !text.glyphs.is_empty() => Some(text),
      _ => None,
    })
    .expect("expected a Text item with glyphs")
}

#[test]
fn sideways_rl_text_baseline_maps_from_block_start_edge() {
  let mut style = ComputedStyle::default();
  style.writing_mode = WritingMode::SidewaysRl;
  style.direction = Direction::Ltr;
  style.font_size = 20.0;
  let style = Arc::new(style);

  let rect = Rect::from_xywh(10.0, 20.0, 100.0, 50.0);
  let baseline_offset = 15.0;

  let list = build_text_display_list(Arc::clone(&style), rect, baseline_offset);
  let text = first_text_item(&list);

  // In `sideways-rl`, the block axis is horizontal and block-start is the right edge of the
  // fragment. `baseline_offset` is measured from block-start, so the physical baseline X position
  // must be computed from `rect.max_x()`.
  let expected_x = rect.x() + rect.width() - baseline_offset;
  let expected_y = rect.y();
  assert!(
    (text.origin.x - expected_x).abs() < 0.01 && (text.origin.y - expected_y).abs() < 0.01,
    "expected Text origin ({expected_x:.2}, {expected_y:.2}), got ({:.2}, {:.2})",
    text.origin.x,
    text.origin.y
  );
}

#[test]
fn sideways_lr_text_origin_uses_inline_start_edge() {
  let mut style = ComputedStyle::default();
  style.writing_mode = WritingMode::SidewaysLr;
  // In `sideways-lr`, `direction` flips the inline-start/inline-end edges even though the inline
  // axis is vertical; the default `ltr` means inline-start is the *bottom* edge.
  style.direction = Direction::Ltr;
  style.font_size = 20.0;
  let style = Arc::new(style);

  let rect = Rect::from_xywh(10.0, 20.0, 100.0, 50.0);
  let baseline_offset = 15.0;

  let list = build_text_display_list(Arc::clone(&style), rect, baseline_offset);
  let text = first_text_item(&list);

  let expected_x = rect.x() + baseline_offset;
  let expected_y = rect.y() + rect.height();
  assert!(
    (text.origin.x - expected_x).abs() < 0.01 && (text.origin.y - expected_y).abs() < 0.01,
    "expected Text origin ({expected_x:.2}, {expected_y:.2}), got ({:.2}, {:.2})",
    text.origin.x,
    text.origin.y
  );
}
