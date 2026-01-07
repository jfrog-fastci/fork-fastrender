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

fn render_placeholder_with_backend(backend: &str) -> Pixmap {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    backend.to_string(),
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
  renderer
    .render_html(html, 200, 80)
    .expect("render placeholder")
}

fn assert_placeholder_opacity_applied(pixmap: &Pixmap, backend: &str) {
  // Fully covered glyph pixels should retain low alpha (≈0.2) instead of painting as fully opaque.
  let max_alpha = max_red_dominant_alpha(pixmap);
  assert!(
    max_alpha > 0,
    "expected placeholder glyph pixels to paint (backend={backend}, max red-dominant alpha={max_alpha})"
  );
  assert!(
    max_alpha < 100,
    "expected placeholder alpha to be reduced by ::placeholder opacity (backend={backend}, max red-dominant alpha={max_alpha})"
  );
}

#[test]
fn display_list_placeholder_pseudo_opacity_is_applied() {
  let pixmap = render_placeholder_with_backend("display_list");
  assert_placeholder_opacity_applied(&pixmap, "display_list");
}

#[test]
fn legacy_placeholder_pseudo_opacity_is_applied() {
  let pixmap = render_placeholder_with_backend("legacy");
  assert_placeholder_opacity_applied(&pixmap, "legacy");
}

