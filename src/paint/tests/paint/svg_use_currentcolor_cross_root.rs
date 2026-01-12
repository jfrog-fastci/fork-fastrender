use crate::image_loader::ImageCache;
use crate::paint::display_list_renderer::PaintParallelism;
use crate::paint::painter::{paint_tree_with_resources_scaled_offset_backend, PaintBackend};
use crate::scroll::ScrollState;
use crate::{FastRender, FontConfig, Point, Rgba};

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).expect("pixel in bounds");
  (p.red(), p.green(), p.blue(), p.alpha())
}

#[test]
fn inline_svg_use_cross_root_preserves_current_color() {
  // A common pattern on real pages is a hidden SVG sprite sheet that defines <symbol> elements,
  // which are referenced from separate <svg><use href="#id"> instances.
  //
  // Many sprites use fill="currentColor"/stroke="currentColor" and rely on inheriting the
  // referencing SVG's computed `color`. Ensure we don't freeze currentColor to the sprite sheet.
  let html = r##"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: white; color: rgb(255, 0, 0); }
      svg { display: block; }
    </style>

    <!-- Hidden sprite sheet -->
    <svg style="display:none">
      <symbol id="icon" viewBox="0 0 10 10">
        <use xlink:href="#shape"></use>
      </symbol>
      <rect id="shape" x="0" y="0" width="10" height="10" fill="currentColor"></rect>
    </svg>

    <!-- Visible icons: should inherit their own color, not the document/sprite color -->
    <svg width="10" height="10" viewBox="0 0 10 10" style="color: rgb(0, 128, 0);">
      <use href="#icon"></use>
    </svg>

    <svg width="10" height="10" viewBox="0 0 10 10" style="color: rgb(0, 0, 255);">
      <use xlink:href="#icon"></use>
    </svg>
  "##;

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");

  let dom = renderer.parse_html(html).expect("parse html");
  let fragments = renderer
    .layout_document(&dom, 10, 20)
    .expect("layout document");

  for backend in [PaintBackend::Legacy, PaintBackend::DisplayList] {
    let pixmap = paint_tree_with_resources_scaled_offset_backend(
      &fragments,
      10,
      20,
      Rgba::WHITE,
      renderer.font_context().clone(),
      ImageCache::new(),
      1.0,
      Point::ZERO,
      PaintParallelism::disabled(),
      &ScrollState::default(),
      backend,
    )
    .expect("paint");

    assert_eq!(
      pixel(&pixmap, 5, 5),
      (0, 128, 0, 255),
      "expected green currentColor when painting via backend={backend:?}",
    );

    assert_eq!(
      pixel(&pixmap, 5, 15),
      (0, 0, 255, 255),
      "expected blue currentColor when painting via backend={backend:?}",
    );
  }
}
