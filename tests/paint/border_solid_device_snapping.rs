use super::util::create_stacking_context_bounds_renderer;

#[test]
fn solid_border_edges_snap_to_device_pixels() {
  // Regression test for solid border rasterization: the display-list renderer previously painted
  // borders as anti-aliased strokes. When layout produced fractional border edges, this blended the
  // border color with the element background, diverging from Chrome/Skia.
  //
  // Ensure that simple axis-aligned solid borders are painted via the canvas rect-fill path (which
  // snaps to device pixels for source-over opaque fills) so the border edge pixel is fully covered.
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: white; }
      .nav {
        width: 195.25px; /* border-start at 195.25px when border-right is 11px */
        height: 40px;
        background: #6BBDD6;
        border-right: 11px solid;
        border-color: #007B9C;
      }
    </style>
    <div class="nav"></div>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer.render_html(html, 240, 60).expect("render");

  let bg = pixmap.pixel(194, 10).expect("bg pixel");
  assert_eq!(
    (bg.red(), bg.green(), bg.blue(), bg.alpha()),
    (107, 189, 214, 255),
    "expected background pixel to match element background"
  );

  let edge = pixmap.pixel(195, 10).expect("edge pixel");
  assert_eq!(
    (edge.red(), edge.green(), edge.blue(), edge.alpha()),
    (0, 123, 156, 255),
    "expected border edge to snap and paint a fully covered pixel"
  );
}
