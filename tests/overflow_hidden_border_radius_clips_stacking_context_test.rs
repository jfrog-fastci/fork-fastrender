use fastrender::geometry::Rect;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::position::Position;
use fastrender::style::types::{BorderCornerRadius, Overflow};
use fastrender::style::values::Length;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::{ComputedStyle, Rgba};
use std::sync::Arc;

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).expect("pixel inside viewport");
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn overflow_hidden_border_radius_clips_stacking_context_children() {
  // Regression coverage for rounded overflow clips in the stacking-context clip chain.
  //
  // `overflow: hidden` establishes a clipping scope (respecting border radii) but does not create a
  // stacking context. If a descendant creates a stacking context, it is promoted to the nearest
  // ancestor stacking context during paint ordering; the rounded clip must still apply.

  let mut parent_style = ComputedStyle::default();
  parent_style.overflow_x = Overflow::Hidden;
  parent_style.overflow_y = Overflow::Hidden;
  let radius = BorderCornerRadius::uniform(Length::px(2.0));
  parent_style.border_top_left_radius = radius;
  parent_style.border_top_right_radius = radius;
  parent_style.border_bottom_right_radius = radius;
  parent_style.border_bottom_left_radius = radius;
  let parent_style = Arc::new(parent_style);

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Relative;
  child_style.z_index = Some(1);
  child_style.background_color = Rgba::RED;
  let child_style = Arc::new(child_style);

  let child =
    FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 4.0, 4.0), vec![], child_style);
  let parent = FragmentNode::new_block_styled(
    Rect::from_xywh(1.0, 1.0, 4.0, 4.0),
    vec![child],
    parent_style,
  );
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 6.0, 6.0), vec![parent]);

  let list = DisplayListBuilder::new().build_with_stacking_tree(&root);
  let pixmap = DisplayListRenderer::new(6, 6, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  // Corner outside the rounded clip stays white while the center paints red.
  assert_ne!(pixel(&pixmap, 1, 1), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 3, 3), (255, 0, 0, 255));
}

