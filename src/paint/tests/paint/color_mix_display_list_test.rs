use crate::geometry::Rect;
use crate::paint::display_list_builder::DisplayListBuilder;
use crate::paint::display_list_renderer::DisplayListRenderer;
use crate::style::color::Color;
use crate::text::font_loader::FontContext;
use crate::tree::fragment_tree::FragmentNode;
use crate::Rgba;
use std::sync::Arc;

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let idx = (y * pixmap.width() + x) as usize * 4;
  (
    pixmap.data()[idx],
    pixmap.data()[idx + 1],
    pixmap.data()[idx + 2],
    pixmap.data()[idx + 3],
  )
}

fn render_background(color: &str) -> (u8, u8, u8, u8) {
  let mut style = crate::ComputedStyle::default();
  style.background_color = Color::parse(color).unwrap().to_rgba(Rgba::BLACK);

  let fragment =
    FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![], Arc::new(style));

  let list = DisplayListBuilder::new().build(&fragment);
  let pixmap = DisplayListRenderer::new(1, 1, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  pixel(&pixmap, 0, 0)
}

#[test]
fn color_mix_srgb_matches_resolved_color() {
  let expected = Color::parse("color-mix(in srgb, red 25%, blue 75%)")
    .unwrap()
    .to_rgba(Rgba::BLACK);
  assert_eq!(
    render_background("color-mix(in srgb, red 25%, blue 75%)"),
    (expected.r, expected.g, expected.b, expected.alpha_u8())
  );
}

#[test]
fn color_mix_srgb_linear_matches_resolved_color() {
  let expected = Color::parse("color-mix(in srgb-linear, red 50%, blue 50%)")
    .unwrap()
    .to_rgba(Rgba::BLACK);
  assert_eq!(
    render_background("color-mix(in srgb-linear, red 50%, blue 50%)"),
    (expected.r, expected.g, expected.b, expected.alpha_u8())
  );
}

#[test]
fn color_mix_uses_current_color() {
  let mut style = crate::ComputedStyle::default();
  style.color = Color::parse("red").unwrap().to_rgba(Rgba::BLACK);
  style.background_color = Color::parse("color-mix(in srgb, currentColor 50%, blue 50%)")
    .unwrap()
    .to_rgba(style.color);

  let fragment =
    FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![], Arc::new(style));

  let list = DisplayListBuilder::new().build(&fragment);
  let pixmap = DisplayListRenderer::new(1, 1, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  let expected = Color::parse("color-mix(in srgb, red 50%, blue 50%)")
    .unwrap()
    .to_rgba(Rgba::BLACK);
  assert_eq!(
    pixel(&pixmap, 0, 0),
    (expected.r, expected.g, expected.b, expected.alpha_u8())
  );
}
