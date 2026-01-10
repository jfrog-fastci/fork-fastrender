use super::util::create_stacking_context_bounds_renderer;

#[test]
fn unscaled_image_snapping_does_not_overpaint_border() {
  // Regression test for unscaled image snapping in the display-list renderer.
  //
  // `DisplayListRenderer` snaps 1:1 image draws to device pixels to match Chrome's rasterization
  // grid and avoid blurry sampling. The previous implementation snapped by truncating the origin,
  // which biased images left/up when the layout position was fractional. For bordered images that
  // meant the snapped image could overlap and overpaint the left/top border pixels.
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: #800080; }
      img {
        display: block;
        margin-left: 9.8px;
        margin-top: 10px;
        border: 1px solid #fff;
      }
    </style>
    <img
      src="data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' width='18' height='18'><rect width='18' height='18' fill='%23ff6600'/></svg>"
      width="18"
      height="18"
      alt=""
    />
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer.render_html(html, 40, 40).expect("render");

  let bg = pixmap.pixel(9, 20).expect("bg pixel");
  assert_eq!(
    (bg.red(), bg.green(), bg.blue(), bg.alpha()),
    (128, 0, 128, 255),
    "expected body background to be visible outside the element"
  );

  // The image border box starts at x=9.8 with a 1px border. Border rasterization snaps to x=10,
  // so pixel (10, 20) should be fully covered by the white left border.
  let border = pixmap.pixel(10, 20).expect("border pixel");
  assert_eq!(
    (border.red(), border.green(), border.blue(), border.alpha()),
    (255, 255, 255, 255),
    "expected the left border pixel to remain visible"
  );

  let inside = pixmap.pixel(12, 20).expect("inside pixel");
  assert_eq!(
    (inside.red(), inside.green(), inside.blue(), inside.alpha()),
    (255, 102, 0, 255),
    "expected the image content to render on the correct pixel grid"
  );
}

