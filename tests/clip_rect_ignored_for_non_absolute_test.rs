use fastrender::geometry::Rect;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::position::Position;
use fastrender::style::types::{ClipComponent, ClipRect};
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::{ComputedStyle, Length, Rgba};
use std::sync::Arc;

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).expect("pixel inside viewport");
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn clip_rect_is_ignored_unless_absolutely_positioned() {
  // CSS 2.1 `clip` only applies to absolutely positioned elements (absolute/fixed). When authors
  // specify `clip` on other positioning schemes the used value should be `auto` (no clip).

  let mut parent_style = ComputedStyle::default();
  parent_style.position = Position::Relative;
  parent_style.clip = Some(ClipRect {
    top: ClipComponent::Length(Length::px(0.0)),
    right: ClipComponent::Length(Length::px(2.0)),
    bottom: ClipComponent::Length(Length::px(4.0)),
    left: ClipComponent::Length(Length::px(0.0)),
  });
  let parent_style = Arc::new(parent_style);

  let mut child_style = ComputedStyle::default();
  child_style.background_color = Rgba::RED;
  let child_style = Arc::new(child_style);

  let child =
    FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 4.0, 4.0), vec![], child_style);

  let parent = FragmentNode::new_block_styled(
    Rect::from_xywh(2.0, 2.0, 4.0, 4.0),
    vec![child],
    parent_style,
  );

  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 8.0, 8.0), vec![parent]);

  let list = DisplayListBuilder::new().build_with_stacking_tree(&root);
  let pixmap = DisplayListRenderer::new(8, 8, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  // Outside the parent box remains white.
  assert_eq!(pixel(&pixmap, 1, 1), (255, 255, 255, 255));
  // Inside the child paints red (clip ignored because parent isn't absolute/fixed).
  assert_eq!(pixel(&pixmap, 3, 3), (255, 0, 0, 255));
  // This pixel would be clipped if `clip` incorrectly applied to relative positioning.
  assert_eq!(pixel(&pixmap, 5, 3), (255, 0, 0, 255));
}

