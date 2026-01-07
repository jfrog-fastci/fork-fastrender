use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;
use tiny_skia::Pixmap;

fn pixel_rgba(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

#[test]
fn legacy_range_slider_thumb_pseudo_style_affects_paint() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "legacy".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  // Use a font-relative length for the thumb size (2em) so the legacy backend must resolve it via
  // the normal length resolution path (not just absolute px values).
  let html = r#"
    <!doctype html>
    <style>
      html, body { margin: 0; background: rgb(0, 0, 0); }
      input.range {
        display: block;
        margin: 0;
        width: 100px;
        height: 20px;
        font-size: 10px;
      }
      input.range::-webkit-slider-thumb {
        width: 2em;
        height: 2em;
        background: rgb(255, 0, 0);
        border: none;
      }
    </style>
    <input class="range" type="range" value="0" min="0" max="100">
  "#;

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  let pixmap = renderer.render_html(html, 120, 40).expect("render slider");

  // The thumb should be 2em (20px) wide, so at `value=0` it spans x=[0..20). Ensure we see the
  // author-specified red thumb color at a pixel that would fall outside the UA default 16px thumb.
  let (r, g, b, a) = pixel_rgba(&pixmap, 17, 10);
  assert!(
    a > 200 && r > 200 && g < 80 && b < 80,
    "expected author ::-webkit-slider-thumb style to affect legacy paint backend; got rgba=({r},{g},{b},{a})"
  );
}
