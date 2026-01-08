use std::sync::Arc;

use fastrender::css::types::Transform;
use fastrender::geometry::Rect;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::types::BackfaceVisibility;
use fastrender::style::values::Length;
use fastrender::ComputedStyle;
use fastrender::FragmentNode;
use fastrender::Rgba;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::FragmentTree;

#[test]
fn backface_hidden_fragments_are_not_painted() {
  let mut style = ComputedStyle::default();
  style.backface_visibility = BackfaceVisibility::Hidden;
  style.transform.push(Transform::RotateY(180.0));
  style.background_color = Rgba::RED;
  style.border_top_width = Length::px(0.0);
  style.border_right_width = Length::px(0.0);
  style.border_bottom_width = Length::px(0.0);
  style.border_left_width = Length::px(0.0);

  let fragment = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
    vec![],
    Arc::new(style),
  );

  // Transforms participate in stacking contexts. Build via the stacking-aware display list so the
  // renderer can apply backface culling at paint time.
  let tree = FragmentTree::new(fragment);
  let list = DisplayListBuilder::new().build_tree_with_stacking(&tree);

  let pixmap = DisplayListRenderer::new(30, 30, Rgba::WHITE, FontContext::new())
    .expect("renderer")
    .render(&list)
    .expect("render");

  assert!(
    (0..pixmap.height())
      .flat_map(|y| (0..pixmap.width()).map(move |x| (x, y)))
      .all(|(x, y)| {
        let px = pixmap.pixel(x, y).expect("pixel in bounds");
        px.red() == 255 && px.green() == 255 && px.blue() == 255 && px.alpha() == 255
      }),
    "backface-hidden fragments facing away should not paint any pixels"
  );
}
