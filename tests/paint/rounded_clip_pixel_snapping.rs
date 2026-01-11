use super::util::create_stacking_context_bounds_renderer;

#[test]
fn rounded_overflow_clip_does_not_bleed_into_adjacent_pixels() {
  // Regression for netflix.com: rounded overflow clips (border-radius + overflow hidden) landed on
  // fractional device pixels and produced 1px blended seams outside the clipped content.
  //
  // For axis-aligned integer-sized clips, we snap the clip origin to the nearest device pixel so
  // pixels immediately outside the clip remain unchanged.
  let html = r#"
    <style>
      html, body { margin: 0; background: #fff; }
      #clipper {
        position: absolute;
        left: 10px;
        top: 10.6px;
        width: 30px;
        height: 30px;
        border-radius: 8px;
        overflow: hidden;
      }
      #inner {
        width: 100%;
        height: 100%;
        background: #f00;
      }
    </style>
    <div id="clipper"><div id="inner"></div></div>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer.render_html(html, 60, 60).expect("render");

  // The clip's top edge is at y=10.6, so scanline y=10 is outside. Without snapping the rounded
  // clip mask, the anti-aliased edge partially covers this row.
  let outside = pixmap.pixel(20, 10).expect("outside pixel");
  assert_eq!(
    (outside.red(), outside.green(), outside.blue(), outside.alpha()),
    (255, 255, 255, 255),
    "expected pixel above rounded clip to remain the page background"
  );
}

