use super::util::bounding_box_for_color;
use crate::debug::runtime::RuntimeToggles;
use crate::{FastRender, RenderOptions};
use std::collections::HashMap;
use tiny_skia::Pixmap;

fn render_html_with_svg_document_css_injection_disabled(
  renderer: &mut FastRender,
  html: &str,
  width: u32,
  height: u32,
) -> Pixmap {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_SVG_EMBED_DOCUMENT_CSS".to_string(),
    "0".to_string(),
  )]));
  let options = RenderOptions::new()
    .with_viewport(width, height)
    .with_runtime_toggles(toggles);
  renderer
    .render_html_with_options(html, options)
    .expect("render svg")
}

fn non_white_bounds(pixmap: &Pixmap) -> (u32, u32, u32, u32) {
  bounding_box_for_color(pixmap, |(r, g, b, a)| {
    a > 0 && !(r == 255 && g == 255 && b == 255)
  })
  .expect("expected some non-white pixels to be painted")
}

#[test]
fn inline_svg_dominant_baseline_changes_output_when_document_css_injection_disabled() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");

      let html_a = r#"
        <style>
          body { margin: 0; background: white; }
          svg { display: block; }
        </style>
        <svg width="120" height="80" viewBox="0 0 120 80">
          <text x="10" y="50" font-family="Cantarell" font-size="40" fill="black">Hi</text>
        </svg>
      "#;

      let html_b = r#"
        <style>
          body { margin: 0; background: white; }
          svg { display: block; dominant-baseline: central; }
        </style>
        <svg width="120" height="80" viewBox="0 0 120 80">
          <text x="10" y="50" font-family="Cantarell" font-size="40" fill="black">Hi</text>
        </svg>
      "#;

      let pixmap_a = render_html_with_svg_document_css_injection_disabled(&mut renderer, html_a, 120, 80);
      let pixmap_b = render_html_with_svg_document_css_injection_disabled(&mut renderer, html_b, 120, 80);

      let bounds_a = non_white_bounds(&pixmap_a);
      let bounds_b = non_white_bounds(&pixmap_b);
      let min_y_delta = (bounds_a.1 as i32 - bounds_b.1 as i32).abs();

      assert_ne!(
        pixmap_a.data(),
        pixmap_b.data(),
        "expected dominant-baseline to affect rendered output when SVG document CSS injection is disabled (bounds_a={bounds_a:?}, bounds_b={bounds_b:?})"
      );
      assert!(
        min_y_delta >= 2,
        "expected dominant-baseline to change the painted bounds (bounds_a={bounds_a:?}, bounds_b={bounds_b:?}, min_y_delta={min_y_delta})"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_baseline_shift_changes_output_when_document_css_injection_disabled() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");

      let html_a = r#"
        <style>
          body { margin: 0; background: white; }
          svg { display: block; }
        </style>
        <svg width="120" height="80" viewBox="0 0 120 80">
          <text x="10" y="55" font-family="Cantarell" font-size="40" fill="black">Hi</text>
        </svg>
      "#;

      let html_b = r#"
        <style>
          body { margin: 0; background: white; }
          svg { display: block; }
          text { baseline-shift: 10px; }
        </style>
        <svg width="120" height="80" viewBox="0 0 120 80">
          <text x="10" y="55" font-family="Cantarell" font-size="40" fill="black">Hi</text>
        </svg>
      "#;

      let pixmap_a = render_html_with_svg_document_css_injection_disabled(&mut renderer, html_a, 120, 80);
      let pixmap_b = render_html_with_svg_document_css_injection_disabled(&mut renderer, html_b, 120, 80);

      let bounds_a = non_white_bounds(&pixmap_a);
      let bounds_b = non_white_bounds(&pixmap_b);
      let min_y_delta = (bounds_a.1 as i32 - bounds_b.1 as i32).abs();

      assert_ne!(
        pixmap_a.data(),
        pixmap_b.data(),
        "expected baseline-shift to affect rendered output when SVG document CSS injection is disabled (bounds_a={bounds_a:?}, bounds_b={bounds_b:?})"
      );
      assert!(
        min_y_delta >= 2,
        "expected baseline-shift to change the painted bounds (bounds_a={bounds_a:?}, bounds_b={bounds_b:?}, min_y_delta={min_y_delta})"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

