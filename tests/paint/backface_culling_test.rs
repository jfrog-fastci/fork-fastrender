use std::sync::Arc;

use fastrender::css::types::Transform;
use fastrender::geometry::Rect;
use fastrender::paint::display_list::{
  BlendMode, BorderRadii, DisplayItem, DisplayList, FillRectItem, StackingContextItem, Transform3D,
};
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::paint::optimize::DisplayListOptimizer;
use fastrender::style::display::Display;
use fastrender::style::types::BackfaceVisibility;
use fastrender::style::types::TransformStyle;
use fastrender::style::values::Length;
use fastrender::ComputedStyle;
use fastrender::FragmentNode;
use fastrender::Rgba;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::FragmentTree;

#[test]
fn backface_hidden_fragments_are_not_painted() {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
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

#[test]
fn backface_hidden_stacking_context_survives_optimization() {
  // Regression test: `backface-visibility: hidden` is represented by a stacking context so the
  // renderer can cull planes whose backface faces away. The display list optimizer must not treat
  // such a stacking context as a no-op, otherwise common card-flip patterns paint incorrectly.
  let flip_transform =
    Transform3D::translate(10.0, 0.0, 0.0).multiply(&Transform3D::rotate_y(std::f32::consts::PI));
  let rect = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);

  let mut list = DisplayList::new();
  list.push(DisplayItem::PushStackingContext(StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: false,
    has_backdrop_sensitive_descendants: false,
    bounds: rect,
    plane_rect: rect,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: false,
    transform: Some(flip_transform),
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: None,
    has_clip_path: false,
  }));

  list.push(DisplayItem::PushStackingContext(StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: false,
    has_backdrop_sensitive_descendants: false,
    bounds: rect,
    plane_rect: rect,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: false,
    transform: None,
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Hidden,
    filters: Vec::new(),
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: None,
    has_clip_path: false,
  }));

  list.push(DisplayItem::FillRect(FillRectItem {
    rect,
    color: Rgba::RED,
  }));

  list.push(DisplayItem::PopStackingContext);
  list.push(DisplayItem::PopStackingContext);

  let viewport = Rect::from_xywh(0.0, 0.0, 30.0, 30.0);
  let (optimized, _stats) = DisplayListOptimizer::new().optimize(list, viewport);

  assert!(
    optimized.items().iter().any(|item| {
      matches!(
        item,
        DisplayItem::PushStackingContext(sc)
          if sc.backface_visibility == BackfaceVisibility::Hidden
      )
    }),
    "optimizer should preserve backface-visibility:hidden stacking contexts"
  );

  let pixmap = DisplayListRenderer::new(30, 30, Rgba::WHITE, FontContext::new())
    .expect("renderer")
    .render(&optimized)
    .expect("render");

  assert!(
    (0..pixmap.height())
      .flat_map(|y| (0..pixmap.width()).map(move |x| (x, y)))
      .all(|(x, y)| {
        let px = pixmap.pixel(x, y).expect("pixel in bounds");
        px.red() == 255 && px.green() == 255 && px.blue() == 255 && px.alpha() == 255
      }),
    "backface-hidden stacking contexts should still be culled after optimization"
  );
}

#[test]
fn backface_hidden_without_3d_context_does_not_reorder_paint() {
  // Regression test: `backface-visibility: hidden` should only promote to a stacking context in a
  // 3D rendering context. In a normal 2D context it must not reorder paint.
  let mut style_a = ComputedStyle::default();
  style_a.display = Display::Block;
  style_a.backface_visibility = BackfaceVisibility::Hidden;
  style_a.background_color = Rgba::RED;
  style_a.border_top_width = Length::px(0.0);
  style_a.border_right_width = Length::px(0.0);
  style_a.border_bottom_width = Length::px(0.0);
  style_a.border_left_width = Length::px(0.0);

  let mut style_b = ComputedStyle::default();
  style_b.display = Display::Block;
  style_b.background_color = Rgba::BLUE;
  style_b.border_top_width = Length::px(0.0);
  style_b.border_right_width = Length::px(0.0);
  style_b.border_bottom_width = Length::px(0.0);
  style_b.border_left_width = Length::px(0.0);

  let fragment_a = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
    vec![],
    Arc::new(style_a),
  );
  let fragment_b = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
    vec![],
    Arc::new(style_b),
  );

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.border_top_width = Length::px(0.0);
  root_style.border_right_width = Length::px(0.0);
  root_style.border_bottom_width = Length::px(0.0);
  root_style.border_left_width = Length::px(0.0);
  let root = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
    vec![fragment_a, fragment_b],
    Arc::new(root_style),
  );

  let tree = FragmentTree::new(root);
  let list = DisplayListBuilder::new().build_tree_with_stacking(&tree);
  let pixmap = DisplayListRenderer::new(10, 10, Rgba::WHITE, FontContext::new())
    .expect("renderer")
    .render(&list)
    .expect("render");

  let p = pixmap.pixel(5, 5).expect("pixel in bounds");
  assert_eq!(
    (p.red(), p.green(), p.blue(), p.alpha()),
    (0, 0, 255, 255),
    "expected later sibling to paint on top (blue). backface-visibility must not reorder paint in 2D contexts"
  );
}

#[test]
fn preserve_3d_card_flip_culls_backface_hidden_children_without_transforms() {
  // Regression test: common card-flip pattern. The parent is rotated in 3D, but the front face
  // has no transform of its own. Its `backface-visibility: hidden` should still be respected.
  let mut card_style = ComputedStyle::default();
  card_style.display = Display::Block;
  card_style.transform_style = TransformStyle::Preserve3d;
  card_style.transform.push(Transform::RotateY(180.0));
  card_style.border_top_width = Length::px(0.0);
  card_style.border_right_width = Length::px(0.0);
  card_style.border_bottom_width = Length::px(0.0);
  card_style.border_left_width = Length::px(0.0);

  let mut front_style = ComputedStyle::default();
  front_style.display = Display::Block;
  front_style.backface_visibility = BackfaceVisibility::Hidden;
  front_style.background_color = Rgba::RED;
  front_style.border_top_width = Length::px(0.0);
  front_style.border_right_width = Length::px(0.0);
  front_style.border_bottom_width = Length::px(0.0);
  front_style.border_left_width = Length::px(0.0);

  let mut back_style = ComputedStyle::default();
  back_style.display = Display::Block;
  back_style.backface_visibility = BackfaceVisibility::Hidden;
  back_style.transform.push(Transform::RotateY(180.0));
  back_style.background_color = Rgba::GREEN;
  back_style.border_top_width = Length::px(0.0);
  back_style.border_right_width = Length::px(0.0);
  back_style.border_bottom_width = Length::px(0.0);
  back_style.border_left_width = Length::px(0.0);

  // Order: put "back" first so "front" would paint on top if it isn't culled.
  let back_fragment = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
    vec![],
    Arc::new(back_style),
  );
  let front_fragment = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
    vec![],
    Arc::new(front_style),
  );

  let card_fragment = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
    vec![back_fragment, front_fragment],
    Arc::new(card_style),
  );

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.border_top_width = Length::px(0.0);
  root_style.border_right_width = Length::px(0.0);
  root_style.border_bottom_width = Length::px(0.0);
  root_style.border_left_width = Length::px(0.0);
  let root = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
    vec![card_fragment],
    Arc::new(root_style),
  );

  let tree = FragmentTree::new(root);
  let list = DisplayListBuilder::new().build_tree_with_stacking(&tree);

  let pixmap = DisplayListRenderer::new(10, 10, Rgba::WHITE, FontContext::new())
    .expect("renderer")
    .render(&list)
    .expect("render");

  let p = pixmap.pixel(5, 5).expect("pixel in bounds");
  assert_eq!(
    (p.red(), p.green(), p.blue(), p.alpha()),
    (0, 255, 0, 255),
    "expected back face to be visible after card flip; front face should be culled by ancestor rotation"
  );
}
