use fastrender::geometry::Rect;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::types::Containment;
use fastrender::style::values::Length;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::ComputedStyle;
use fastrender::Rgba;
use std::sync::Arc;

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).expect("pixel inside viewport");
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn paint_containment_clips_descendants_to_padding_box() {
  let mut parent_style = ComputedStyle::default();
  parent_style.containment = Containment::with_flags(false, false, false, false, true);
  parent_style.padding_left = Length::px(1.0);
  parent_style.padding_right = Length::px(1.0);
  parent_style.padding_top = Length::px(1.0);
  parent_style.padding_bottom = Length::px(1.0);
  let parent_style = Arc::new(parent_style);

  let mut child_style = ComputedStyle::default();
  child_style.background_color = Rgba::RED;
  let child_style = Arc::new(child_style);

  // Child overflows the parent's padding box in all directions so we can validate that paint
  // containment clips descendants at the padding edge (not the content edge).
  let child =
    FragmentNode::new_block_styled(Rect::from_xywh(-1.0, -1.0, 8.0, 8.0), vec![], child_style);
  let parent = FragmentNode::new_block_styled(
    Rect::from_xywh(1.0, 1.0, 6.0, 6.0),
    vec![child],
    parent_style,
  );
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 8.0, 8.0), vec![parent]);

  let list = DisplayListBuilder::new().build_with_stacking_tree(&root);
  let pixmap = DisplayListRenderer::new(8, 8, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  // Paint containment clips at the padding edge: with 1px padding the padding box is still the
  // border box (no borders), so descendants can paint into padding but not outside the element.
  assert_eq!(pixel(&pixmap, 0, 3), (255, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 1, 3), (255, 0, 0, 255));
}
