use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::{paint_tree_with_resources_scaled_offset_backend, PaintBackend};
use fastrender::scroll::ScrollState;
use fastrender::{FastRender, Point, Rgba};

fn render(html: &str, width: u32, height: u32, backend: PaintBackend) -> tiny_skia::Pixmap {
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parsed");
  let fragment_tree = renderer
    .layout_document(&dom, width, height)
    .expect("laid out");

  let font_ctx = renderer.font_context().clone();
  let image_cache = ImageCache::new();

  paint_tree_with_resources_scaled_offset_backend(
    &fragment_tree,
    width,
    height,
    Rgba::TRANSPARENT,
    font_ctx,
    image_cache,
    1.0,
    Point::ZERO,
    PaintParallelism::disabled(),
    &ScrollState::default(),
    backend,
  )
  .expect("painted")
}

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

#[test]
fn range_slider_track_and_thumb_pseudo_opacity_is_applied() {
  let html = r#"
    <!doctype html>
    <style>
      html, body { margin: 0; background: transparent; }
      #slider {
        position: absolute;
        left: 0;
        top: 0;
        width: 100px;
        height: 20px;
        padding: 0;
        border: 0;
        appearance: none;
        background: transparent;
      }
      #slider::-webkit-slider-runnable-track,
      #slider::-moz-range-track {
        height: 6px;
        background: rgb(255 0 0);
        opacity: 0.2;
      }
      #slider::-webkit-slider-thumb,
      #slider::-moz-range-thumb {
        width: 10px;
        height: 10px;
        border: 0;
        background: rgb(0 0 255);
        opacity: 0.2;
      }
    </style>
    <input id="slider" type="range" value="0" min="0" max="100" />
  "#;

  for backend in [PaintBackend::DisplayList, PaintBackend::Legacy] {
    let pixmap = render(html, 120, 40, backend);

    // A track pixel far from the thumb should retain low alpha due to the track pseudo-element
    // opacity.
    let (r, g, b, a) = pixel(&pixmap, 50, 10);
    assert!(
      a > 0 && a < 100,
      "expected track alpha to be reduced by pseudo-element opacity (backend={backend:?}, rgba=({r},{g},{b},{a}))"
    );
    assert!(
      r > g.saturating_add(20) && r > b.saturating_add(20),
      "expected track pixel to be red-dominant (backend={backend:?}, rgba=({r},{g},{b},{a}))"
    );

    // Sample a thumb pixel outside the track rect so we observe thumb opacity directly (without
    // blending against the track).
    let (r, g, b, a) = pixel(&pixmap, 5, 6);
    assert!(
      a > 0 && a < 100,
      "expected thumb alpha to be reduced by pseudo-element opacity (backend={backend:?}, rgba=({r},{g},{b},{a}))"
    );
    assert!(
      b > r.saturating_add(20) && b > g.saturating_add(20),
      "expected thumb pixel to be blue-dominant (backend={backend:?}, rgba=({r},{g},{b},{a}))"
    );
  }
}
