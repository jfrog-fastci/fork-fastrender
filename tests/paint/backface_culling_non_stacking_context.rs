use std::sync::Arc;

use fastrender::css::types::Transform;
use fastrender::geometry::Rect;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::types::BackfaceVisibility;
use fastrender::style::values::Length;
use fastrender::text::font_loader::FontContext;
use fastrender::ComputedStyle;
use fastrender::FragmentNode;
use fastrender::Rgba;

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).expect("pixel in bounds");
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn backface_hidden_non_stacking_elements_are_culled_by_ancestor_transform() {
  // Parent establishes the 3D transform (stacking context trigger via `transform`).
  let mut parent_style = ComputedStyle::default();
  parent_style.transform.push(Transform::RotateY(180.0));
  parent_style.border_top_width = Length::px(0.0);
  parent_style.border_right_width = Length::px(0.0);
  parent_style.border_bottom_width = Length::px(0.0);
  parent_style.border_left_width = Length::px(0.0);

  // Child is *not* a stacking context (no transform/z-index/etc), but should still be culled due to
  // `backface-visibility: hidden`.
  let mut child_style = ComputedStyle::default();
  child_style.backface_visibility = BackfaceVisibility::Hidden;
  child_style.background_color = Rgba::RED;
  child_style.border_top_width = Length::px(0.0);
  child_style.border_right_width = Length::px(0.0);
  child_style.border_bottom_width = Length::px(0.0);
  child_style.border_left_width = Length::px(0.0);
  let child = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
    vec![],
    Arc::new(child_style),
  );

  let parent = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
    vec![child],
    Arc::new(parent_style),
  );

  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 30.0, 30.0), vec![parent]);
  let list = DisplayListBuilder::new().build_with_stacking_tree(&root);

  let pixmap = DisplayListRenderer::new(30, 30, Rgba::WHITE, FontContext::new())
    .expect("renderer")
    .render(&list)
    .expect("render");

  // Child area should remain white (culled).
  assert_eq!(pixel(&pixmap, 5, 5), (255, 255, 255, 255));
  // And the rest of the canvas stays white.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 255, 255, 255));
}

