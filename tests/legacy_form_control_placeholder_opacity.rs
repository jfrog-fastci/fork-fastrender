use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig, Rgba};
use std::collections::HashMap;
use tiny_skia::Pixmap;

fn max_red_dominant_alpha(pixmap: &Pixmap) -> u8 {
  pixmap
    .pixels()
    .iter()
    .filter_map(|px| {
      let a = px.alpha();
      if a == 0 {
        return None;
      }
      let r = px.red();
      let g = px.green();
      let b = px.blue();
      let red_dominant = r.saturating_sub(g) > 32 && r.saturating_sub(b) > 32;
      red_dominant.then_some(a)
    })
    .max()
    .unwrap_or(0)
}

#[test]
fn legacy_placeholder_pseudo_opacity_is_applied() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "legacy".to_string(),
  )]));
  let config = FastRenderConfig::new()
    .with_default_background(Rgba::TRANSPARENT)
    .with_runtime_toggles(toggles);

  // Use a transparent canvas so we can observe the placeholder text alpha directly. Setting
  // `opacity` on `::placeholder` should multiply the text color alpha in both paint backends.
  let html = r#"<!doctype html>
    <style>
      html,body{margin:0;background:transparent;}
      input{
        display:block;
        margin:0;
        width:200px;
        height:60px;
        background:transparent;
        border:0;
        padding:0;
        color:rgb(0,255,0);
        font:50px sans-serif;
        line-height:1;
      }
      input::placeholder{
        color:rgb(255,0,0);
        opacity:0.2;
      }
    </style>
    <input placeholder="MMMM" value="">
  "#;

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  let pixmap = renderer
    .render_html(html, 200, 80)
    .expect("render placeholder");

  // Fully covered glyph pixels should retain low alpha (≈0.2) instead of painting as fully opaque.
  let max_alpha = max_red_dominant_alpha(&pixmap);
  assert!(
    max_alpha > 0,
    "expected placeholder glyph pixels to paint (max red-dominant alpha={max_alpha})"
  );
  assert!(
    max_alpha < 100,
    "expected placeholder alpha to be reduced by ::placeholder opacity (max red-dominant alpha={max_alpha})"
  );
}

