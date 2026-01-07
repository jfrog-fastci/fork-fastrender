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
    Rgba::WHITE,
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
fn range_track_pseudo_element_paints_under_appearance_none() {
  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; background: white; }
      #slider {
        position: absolute;
        left: 0;
        top: 0;
        width: 100px;
        height: 20px;
        padding: 0;
        border: 0;
        appearance: none;
        background: rgb(0 255 0);
      }
      #slider::-webkit-slider-runnable-track,
      #slider::-moz-range-track {
        height: 6px;
        background: rgb(255 0 0);
      }
      #slider::-webkit-slider-thumb,
      #slider::-moz-range-thumb {
        width: 1px;
        height: 1px;
        border: 0;
        background: transparent;
      }
    </style>
    <input id="slider" type="range" value="50" min="0" max="100" />
  "#;

  for backend in [PaintBackend::DisplayList, PaintBackend::Legacy] {
    let pixmap = render(html, 120, 40, backend);
    assert_eq!(
      pixel(&pixmap, 10, 10),
      (255, 0, 0, 255),
      "expected track pseudo-element background to paint over the input background (backend={backend:?})"
    );
    assert_eq!(
      pixel(&pixmap, 10, 2),
      (0, 255, 0, 255),
      "expected input background to remain visible outside the track rect (backend={backend:?})"
    );
  }
}
