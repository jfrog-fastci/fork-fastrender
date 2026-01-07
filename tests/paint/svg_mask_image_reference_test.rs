use fastrender::geometry::Point;
use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::{paint_tree_with_resources_scaled_offset_backend, PaintBackend};
use fastrender::scroll::ScrollState;
use fastrender::style::color::Rgba;
use fastrender::FastRender;
use fastrender::Pixmap;

fn pixel(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).expect("pixel in bounds");
  (p.red(), p.green(), p.blue(), p.alpha())
}

#[test]
fn svg_mask_image_reference_resolves_use_dependencies() {
  // Many real-world pages define masks inside hidden SVG <defs> blocks and then reference them
  // from CSS via `mask-image: url(#id)`. Those masks frequently use `<use xlink:href="#...">`
  // to reference other defs.
  let html = r##"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      #box {
        width: 100px;
        height: 100px;
        background: rgb(255 0 0);
        mask-image: url(#m);
        mask-mode: alpha;
        mask-repeat: no-repeat;
        mask-size: 100% 100%;
        mask-position: 0 0;
      }
    </style>

    <svg style="display: none" xmlns="http://www.w3.org/2000/svg"
         xmlns:xlink="http://www.w3.org/1999/xlink">
      <defs>
        <rect id="shape" x="0" y="0" width="50" height="100" fill="white"/>
        <mask id="m" maskUnits="userSpaceOnUse" maskContentUnits="userSpaceOnUse"
              x="0" y="0" width="100" height="100">
          <use xlink:href="#shape"/>
        </mask>
      </defs>
    </svg>

    <div id="box"></div>
  "##;

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse html");
  let fragments = renderer
    .layout_document(&dom, 100, 100)
    .expect("layout document");

  assert!(
    fragments
      .svg_id_defs
      .as_ref()
      .is_some_and(|defs| defs.contains_key("m") && defs.contains_key("shape")),
    "layout should retain defs required by url(#m) mask-image"
  );

  let pixmap = paint_tree_with_resources_scaled_offset_backend(
    &fragments,
    100,
    100,
    Rgba::WHITE,
    renderer.font_context().clone(),
    ImageCache::new(),
    1.0,
    Point::ZERO,
    PaintParallelism::disabled(),
    &ScrollState::default(),
    PaintBackend::DisplayList,
  )
  .expect("paint");

  // Left half is visible (mask contains a 50px-wide white rect via <use>).
  assert_eq!(pixel(&pixmap, 10, 50), (255, 0, 0, 255));
  // Right half is masked out and shows the canvas background.
  assert_eq!(pixel(&pixmap, 90, 50), (255, 255, 255, 255));
}
