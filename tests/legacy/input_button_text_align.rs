use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;
use tiny_skia::Pixmap;

fn mean_dark_x(pixmap: &Pixmap, x0: u32, y0: u32, x1: u32, y1: u32) -> Option<f32> {
  let mut sum_x: u64 = 0;
  let mut count: u64 = 0;
  for y in y0..y1 {
    for x in x0..x1 {
      let Some(px) = pixmap.pixel(x, y) else {
        continue;
      };
      if px.alpha() < 32 {
        continue;
      }
      let r = px.red() as u32;
      let g = px.green() as u32;
      let b = px.blue() as u32;
      // Relative luminance approximation (0..255).
      let lum = (299 * r + 587 * g + 114 * b) / 1000;
      if lum < 120 {
        sum_x += x as u64;
        count += 1;
      }
    }
  }
  if count == 0 {
    None
  } else {
    Some(sum_x as f32 / count as f32)
  }
}

#[test]
fn legacy_input_buttons_honor_text_align() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "legacy".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  // Use `font-size: 0` on the body so whitespace between the two inputs has no layout width,
  // keeping button boxes at deterministic x positions for this pixel-based assertion.
  let html = r#"<!doctype html>
<style>
  body {
    margin: 0;
    background: white;
    padding: 16px;
    font-family: Arial, sans-serif;
    font-size: 0;
  }

  .btn {
    appearance: none;
    width: 140px;
    height: 36px;
    padding: 0 12px;
    border: 1px solid #333;
    background: #e9e9e9;
    font-size: 16px;
  }

  .btn + .btn {
    margin-left: 16px;
  }

  .left { text-align: left; }
  .right { text-align: right; }
</style>
<input class="btn left" type="button" value="Align"><input class="btn right" type="button" value="Align">
"#;

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  let pixmap = renderer
    .render_html(html, 340, 80)
    .expect("render input buttons");

  // Expected geometry from the HTML/CSS above:
  // - body padding: 16px
  // - button width: 140px
  // - gap via margin-left: 16px
  let pad: u32 = 16;
  let w: u32 = 140;
  let h: u32 = 36;
  let gap: u32 = 16;
  let inset: u32 = 2;
  let left_x0 = pad;
  let right_x0 = pad + w + gap;
  let y0 = pad;

  let left_mean_x = mean_dark_x(
    &pixmap,
    left_x0 + inset,
    y0 + inset,
    left_x0 + w - inset,
    y0 + h - inset,
  )
  .expect("left button should contain dark text pixels");
  let right_mean_x = mean_dark_x(
    &pixmap,
    right_x0 + inset,
    y0 + inset,
    right_x0 + w - inset,
    y0 + h - inset,
  )
  .expect("right button should contain dark text pixels");

  let left_rel = left_mean_x - left_x0 as f32;
  let right_rel = right_mean_x - right_x0 as f32;

  assert!(
    right_rel - left_rel > 10.0,
    "expected text-align to affect horizontal label placement in legacy backend (left_rel={left_rel:.2}, right_rel={right_rel:.2})"
  );
}

