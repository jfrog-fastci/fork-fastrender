use super::util::create_stacking_context_bounds_renderer;

#[test]
fn stacking_context_filter_samples_offscreen_pixels_near_viewport_edge() {
  let mut renderer = create_stacking_context_bounds_renderer();

  // Regression test:
  // A filtered stacking context can contribute pixels inside the viewport even when the element's
  // border box is fully offscreen. We must keep a halo around the viewport when allocating the
  // filtered layer, otherwise the offscreen source pixels are clipped away and the filter output
  // incorrectly disappears at the edge.
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(0, 0, 0); }
      #target {
        position: absolute;
        top: 40px;
        left: -40px;
        width: 20px;
        height: 20px;
        background: rgb(255, 0, 0);
        filter: blur(20px);
      }
    </style>
    <div id="target"></div>
  "#;

  let pixmap = renderer
    .render_html(html, 100, 100)
    .expect("render should succeed");

  // Pixel at the left edge of the viewport should pick up some red from the offscreen element.
  let p = pixmap.pixel(0, 50).expect("pixel inside viewport");
  assert!(
    p.red() > 0 && p.red() > p.green() && p.red() > p.blue() && p.alpha() > 0,
    "expected red blur contribution at viewport edge, got rgba({}, {}, {}, {})",
    p.red(),
    p.green(),
    p.blue(),
    p.alpha()
  );
}

