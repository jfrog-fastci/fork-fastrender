use super::util::create_stacking_context_bounds_renderer;

#[test]
fn solid_border_does_not_bleed_into_adjacent_pixels() {
  // Repro:
  // - Solid 1px borders are drawn as stroked lines.
  // - When the border edge lands on a fractional device pixel (e.g. x=2.8), an anti-aliased
  //   stroke can partially cover the pixel immediately before the border (x=2), producing
  //   unintended blended colors.
  //
  // The renderer should instead paint crisp solid borders for axis-aligned transforms so the
  // pixel outside the border remains unchanged.
  let html = r#"
    <style>
      html, body { margin: 0; background: #ff6600; }
      #box {
        position: absolute;
        left: 2.8px;
        top: 2px;
        width: 18px;
        height: 18px;
        border: 1px solid #fff;
      }
    </style>
    <div id="box"></div>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer.render_html(html, 30, 30).expect("render");

  let outside = pixmap.pixel(2, 2).expect("outside pixel");
  assert_eq!(
    (outside.red(), outside.green(), outside.blue(), outside.alpha()),
    (255, 102, 0, 255),
    "expected pixel outside border to remain the background color"
  );

  let border = pixmap.pixel(3, 2).expect("border pixel");
  assert_eq!(
    (border.red(), border.green(), border.blue(), border.alpha()),
    (255, 255, 255, 255),
    "expected border pixel to be solid white"
  );
}

