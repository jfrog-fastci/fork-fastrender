use fastrender::debug::runtime::RuntimeToggles;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::{FastRender, FastRenderConfig, FontConfig, LayoutParallelism};
use std::collections::HashMap;
use tiny_skia::Pixmap;

fn count_colored_pixels_in_rows(pixmap: &Pixmap, start_y: u32, end_y: u32) -> usize {
  let width = pixmap.width() as usize;
  let height = pixmap.height();
  let data = pixmap.data();
  let start_y = start_y.min(height);
  let end_y = end_y.min(height);
  let mut count = 0usize;
  for y in start_y..end_y {
    let row = &data[y as usize * width * 4..(y as usize + 1) * width * 4];
    count += row
      .chunks_exact(4)
      .filter(|px| {
        let r = px[0];
        let g = px[1];
        let b = px[2];
        let a = px[3];
        a != 0 && (r != g || g != b)
      })
      .count();
  }
  count
}

fn renderer_with_subpixel_aa() -> FastRender {
  crate::rayon_test_util::init_rayon_for_tests(2);
  let toggles = RuntimeToggles::from_map(HashMap::from([
    ("FASTR_PAINT_BACKEND".to_string(), "display_list".to_string()),
    ("FASTR_TEXT_SUBPIXEL_AA".to_string(), "1".to_string()),
  ]));
  let config = FastRenderConfig::new()
    .with_runtime_toggles(toggles)
    .with_font_sources(FontConfig::bundled_only())
    .with_layout_parallelism(LayoutParallelism::disabled())
    .with_paint_parallelism(PaintParallelism::disabled());
  FastRender::with_config(config).expect("renderer")
}

#[test]
fn text_subpixel_aa_scratch_resizes_between_glyph_sizes() {
  // Render two different font sizes in a single pass with subpixel AA enabled. This exercises the
  // scratch pixmap resizing logic used to store the subpixel coverage mask.
  let mut renderer = renderer_with_subpixel_aa();
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; background: rgb(255, 255, 255); }
      #small { position: absolute; left: 10.3px; top: 0px; font-size: 16px; color: rgb(0, 0, 0); }
      #big { position: absolute; left: 10.3px; top: 120px; font-size: 96px; color: rgb(0, 0, 0); }
    </style>
    <div id="small">A</div>
    <div id="big">A</div>
  "#;

  let pixmap = renderer.render_html(html, 240, 240).expect("render html");

  let top_tinted = count_colored_pixels_in_rows(&pixmap, 0, 100);
  let bottom_tinted = count_colored_pixels_in_rows(&pixmap, 100, 240);
  assert!(
    top_tinted > 0,
    "expected subpixel AA to produce tinted edge pixels for the small glyph (top region)"
  );
  assert!(
    bottom_tinted > 0,
    "expected subpixel AA to produce tinted edge pixels for the large glyph (bottom region)"
  );
}

