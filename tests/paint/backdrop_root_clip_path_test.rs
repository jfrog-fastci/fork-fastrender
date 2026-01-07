use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::paint_tree_with_resources_scaled_offset;
use fastrender::scroll::ScrollState;
use fastrender::{FastRender, Point, Rgba};

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn render(html: &str, width: u32, height: u32) -> tiny_skia::Pixmap {
  crate::rayon_test_util::init_rayon_for_tests(2);
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parsed");
  let fragment_tree = renderer
    .layout_document(&dom, width, height)
    .expect("laid out");

  let font_ctx = renderer.font_context().clone();
  let image_cache = ImageCache::new();

  paint_tree_with_resources_scaled_offset(
    &fragment_tree,
    width,
    height,
    Rgba::WHITE,
    font_ctx,
    image_cache,
    1.0,
    Point::ZERO,
    // Keep painting deterministic; this test focuses on Backdrop Root boundaries.
    PaintParallelism::disabled(),
    &ScrollState::default(),
  )
  .expect("painted")
}

#[test]
fn backdrop_filter_does_not_sample_above_clip_path_backdrop_root() {
  let run = |clip_path: &str| {
    let html = format!(
      r#"<!doctype html>
        <style>
          body {{ margin:0; background: rgb(255 0 0); }}
          #cliproot {{ position:absolute; inset:0; clip-path: {clip_path}; }}
          #overlay {{
            position:absolute;
            left:0;
            top:0;
            width:40px;
            height:40px;
            backdrop-filter: invert(1);
          }}
        </style>
        <div id=cliproot><div id=overlay></div></div>
      "#
    );

    let pixmap = render(&html, 60, 60);

    // Pixel inside the backdrop-filter element should remain the body background (red) because the
    // clip-path ancestor establishes a Backdrop Root.
    assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
    // Pixel outside the backdrop-filter element should also remain the body background (red).
    assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
  };

  // `inset(0)` is a no-op clip-path, but still establishes a Backdrop Root per Filter Effects L2.
  run("inset(0px)");
  // Some clip-path values currently resolve to `None` at paint time (e.g. degenerate shapes), but
  // must still establish a Backdrop Root because the computed style is non-`none`.
  run("circle(0px)");
}
