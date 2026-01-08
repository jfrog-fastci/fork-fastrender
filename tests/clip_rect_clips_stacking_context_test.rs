use fastrender::geometry::Rect;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::position::Position;
use fastrender::style::types::{ClipComponent, ClipRect};
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
fn clip_rect_clips_stacking_context_children() {
  // Regression coverage for the stacking-context clip chain:
  //
  // `clip: rect(...)` establishes a clipping scope for descendants without creating a stacking
  // context. When a descendant does create a stacking context, it is promoted to the nearest
  // ancestor stacking context during painting, but the clip must still apply.

  let mut parent_style = ComputedStyle::default();
  parent_style.position = Position::Absolute;
  parent_style.clip = Some(ClipRect {
    top: ClipComponent::Length(Length::px(0.0)),
    right: ClipComponent::Length(Length::px(4.0)),
    bottom: ClipComponent::Length(Length::px(4.0)),
    left: ClipComponent::Length(Length::px(0.0)),
  });
  let parent_style = Arc::new(parent_style);

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Relative;
  child_style.z_index = Some(1);
  child_style.background_color = Rgba::RED;
  let child_style = Arc::new(child_style);

  let child =
    FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 6.0, 6.0), vec![], child_style);
  let parent = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 6.0, 6.0),
    vec![child],
    parent_style,
  );
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 8.0, 8.0), vec![parent]);

  let list = DisplayListBuilder::new().build_with_stacking_tree(&root);
  let pixmap = DisplayListRenderer::new(8, 8, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  // Pixel inside the clip rect paints red.
  assert_eq!(pixel(&pixmap, 2, 2), (255, 0, 0, 255));
  // Pixel inside the child but outside the clip rect stays white.
  assert_eq!(pixel(&pixmap, 5, 2), (255, 255, 255, 255));
}

