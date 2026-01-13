use crate::debug::runtime::RuntimeToggles;
use crate::{FastRender, FastRenderConfig};
use std::collections::HashMap;

#[test]
fn template_element_never_generates_boxes_even_with_display_override() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config(config).expect("renderer should construct");

  // Per the HTML Standard, `<template>` "represents nothing" in a rendering. Author CSS must not be
  // able to force it to generate a box or affect layout.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; background: rgb(0,255,0); }
      template {
        display: block !important;
        width: 50px;
        height: 50px;
        background: rgb(0,0,255);
      }
      #after {
        width: 50px;
        height: 50px;
        background: rgb(255,0,0);
      }
    </style>
    <template></template>
    <div id="after"></div>
  "#;

  let pixmap = renderer
    .render_html(html, 60, 60)
    .expect("render should succeed");

  let pixel = pixmap.pixel(10, 10).expect("pixel");
  assert!(
    pixel.red() > 200 && pixel.green() < 80 && pixel.blue() < 80,
    "expected template to not paint or affect layout (red box should start at y=0); got rgba({}, {}, {}, {})",
    pixel.red(),
    pixel.green(),
    pixel.blue(),
    pixel.alpha()
  );
}
