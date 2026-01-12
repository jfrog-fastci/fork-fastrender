use fastrender::debug::runtime::RuntimeToggles;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::{FastRender, FastRenderConfig, FontConfig, LayoutParallelism};
use std::collections::HashMap;
use tiny_skia::Pixmap;

fn count_colored_pixels(pixmap: &Pixmap) -> usize {
  pixmap
    .data()
    .chunks_exact(4)
    .filter(|px| {
      let r = px[0];
      let g = px[1];
      let b = px[2];
      let a = px[3];
      a != 0 && (r != g || g != b)
    })
    .count()
}

fn renderer_with_subpixel_aa() -> FastRender {
  crate::rayon_test_util::init_rayon_for_tests(2);
  let toggles = RuntimeToggles::from_map(HashMap::from([
    (
      "FASTR_PAINT_BACKEND".to_string(),
      "display_list".to_string(),
    ),
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
fn webkit_font_smoothing_disables_subpixel_aa() {
  let mut renderer = renderer_with_subpixel_aa();
  let base_html = r#"<!doctype html>
    <style>
      html, body { margin: 0; background: rgb(255, 255, 255); }
      #t { position: absolute; left: 10.3px; top: 0; font-size: 32px; color: rgb(0, 0, 0); }
    </style>
    <div id="t">A</div>
  "#;

  let smoothed_html = r#"<!doctype html>
    <style>
      html, body { margin: 0; background: rgb(255, 255, 255); }
      #t {
        position: absolute;
        left: 10.3px;
        top: 0;
        font-size: 32px;
        color: rgb(0, 0, 0);
        -webkit-font-smoothing: antialiased;
      }
    </style>
    <div id="t">A</div>
  "#;

  let base = renderer
    .render_html(base_html, 96, 96)
    .expect("render base html");
  assert!(
    count_colored_pixels(&base) > 0,
    "expected subpixel AA to produce tinted edge pixels for black text when enabled"
  );

  let smoothed = renderer
    .render_html(smoothed_html, 96, 96)
    .expect("render smoothed html");
  assert_eq!(
    count_colored_pixels(&smoothed),
    0,
    "expected -webkit-font-smoothing: antialiased to disable subpixel AA tinting"
  );
}
