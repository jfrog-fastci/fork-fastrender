use super::util::create_stacking_context_bounds_renderer;
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::text::font_loader::FontContext;
use fastrender::{
  BorderRadii, BoxShadowItem, DisplayItem, DisplayList, FontConfig, PaintParallelism, Point, Rect,
  Rgba,
};

#[test]
fn css_box_shadow_blur_radius_matches_display_list_item_semantics() {
  // Regression test for `box-shadow` blur radius handling in the display list builder.
  //
  // A `BoxShadowItem` stores the CSS blur radius; conversion to gaussian sigma must happen only in
  // the rasterizer. If the display list builder converts the blur radius prematurely, the output
  // differs from rendering an equivalent `BoxShadowItem` directly.
  let html = r#"
    <style>
      html, body { margin: 0; background: white; }
      #target {
        position: absolute;
        left: 0;
        top: 0;
        width: 200px;
        height: 20px;
        box-shadow: 0 5px 10px black;
      }
    </style>
    <div id="target"></div>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let html_pixmap = renderer.render_html(html, 200, 60).expect("render html");

  let mut list = DisplayList::new();
  list.push(DisplayItem::BoxShadow(BoxShadowItem {
    rect: Rect::from_xywh(0.0, 0.0, 200.0, 20.0),
    radii: BorderRadii::ZERO,
    offset: Point::new(0.0, 5.0),
    blur_radius: 10.0,
    spread_radius: 0.0,
    color: Rgba::BLACK,
    inset: false,
  }));

  let font_ctx = FontContext::with_config(FontConfig::bundled_only());
  let list_pixmap = DisplayListRenderer::new(200, 60, Rgba::WHITE, font_ctx)
    .expect("display list renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render display list");

  assert_eq!(
    html_pixmap.data(),
    list_pixmap.data(),
    "expected HTML box-shadow output to match rendering a direct BoxShadowItem"
  );
}
