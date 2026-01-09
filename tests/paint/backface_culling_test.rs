use std::sync::Arc;

use fastrender::css::types::Transform;
use fastrender::geometry::Rect;
use fastrender::paint::display_list::{
  BlendMode, BorderRadii, DisplayItem, DisplayList, FillRectItem, StackingContextItem, Transform3D,
};
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::paint::optimize::DisplayListOptimizer;
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
