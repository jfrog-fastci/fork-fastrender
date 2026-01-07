use fastrender::debug::runtime::RuntimeToggles;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::{DiagnosticsLevel, FastRender, RenderOptions};
use std::collections::HashMap;

fn build_html(will_change: bool) -> String {
  let will_change = if will_change {
    "will-change: filter;"
  } else {
    ""
  };
  let mut html = format!(
    r#"<!doctype html>
      <style>
        html, body {{ margin: 0; padding: 0; background: rgb(250 250 250); }}
        #wrapper {{
          position: relative;
          width: 256px;
          height: 256px;
          background: rgb(20 40 60);
          {will_change}
        }}
        .tile {{
          position: absolute;
          width: 16px;
          height: 16px;
        }}
      </style>
      <div id="wrapper">
    "#
  );

  // Populate the wrapper with a sizeable subtree. The tiles are purely solid colors so that
  // painting via an intermediate layer (the naive backdrop root implementation) would still be
  // pixel-identical; the test asserts that no extra layers are allocated instead.
  for i in 0..128u32 {
    let x = (i % 16) * 16;
    let y = (i / 16) * 16;
    let r = (i * 37 % 255) as u8;
    let g = (i * 67 % 255) as u8;
    let b = (i * 97 % 255) as u8;
    html.push_str(&format!(
      r#"<div class="tile" style="left:{x}px;top:{y}px;background:rgb({r} {g} {b})"></div>"#
    ));
  }

  html.push_str("</div>");
  html
}

#[test]
fn will_change_backdrop_root_only_forces_layers_when_needed() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let options = RenderOptions::new()
    .with_viewport(256, 256)
    .with_diagnostics_level(DiagnosticsLevel::Basic)
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_runtime_toggles(toggles);

  let mut renderer = FastRender::new().expect("renderer");

  let baseline = renderer
    .render_html_with_diagnostics(&build_html(false), options.clone())
    .expect("baseline render");
  let will_change = renderer
    .render_html_with_diagnostics(&build_html(true), options)
    .expect("will-change render");

  assert_eq!(
    baseline.pixmap.data(),
    will_change.pixmap.data(),
    "expected `will-change: filter` without backdrop-sensitive descendants to be a paint no-op"
  );

  let baseline_layers = baseline
    .diagnostics
    .stats
    .as_ref()
    .and_then(|stats| stats.paint.layer_allocations)
    .unwrap_or(0);
  let will_change_layers = will_change
    .diagnostics
    .stats
    .as_ref()
    .and_then(|stats| stats.paint.layer_allocations)
    .unwrap_or(0);

  assert_eq!(
    baseline_layers, will_change_layers,
    "expected will-change backdrop roots to avoid forcing extra layers when there are no backdrop-sensitive descendants (baseline={baseline_layers} will_change={will_change_layers})"
  );
}

