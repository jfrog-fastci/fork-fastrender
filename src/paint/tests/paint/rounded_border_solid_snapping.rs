use super::util::create_stacking_context_bounds_renderer;

#[test]
fn solid_rounded_border_snaps_axis_aligned_edges() {
  // Regression test for solid rounded borders: even when the outer border box is snapped to
  // integer device pixels, tiny-skia's anti-aliased path fill can treat boundary samples as
  // inside the border ring. This produces washed-out 1px borders and a faint inner halo when the
  // border box lands on fractional coordinates.
  //
  // Ensure that a 1px solid border renders a fully covered edge pixel without tinting the pixel
  // just inside the border.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; background: #ff6600; }
      #box {
        position: absolute;
        left: 0;
        top: 0.35px;
        width: 20px;
        height: 20px;
        box-sizing: border-box;
        background: #fff;
        border: 1px solid #000;
        border-radius: 3px;
      }
    </style>
    <div id="box"></div>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer.render_html(html, 40, 40).expect("render");

  let border = pixmap.pixel(10, 0).expect("border pixel");
  assert_eq!(
    (border.red(), border.green(), border.blue(), border.alpha()),
    (0, 0, 0, 255),
    "expected top border pixel to be fully covered"
  );

  let inside = pixmap.pixel(10, 1).expect("inside pixel");
  assert_eq!(
    (inside.red(), inside.green(), inside.blue(), inside.alpha()),
    (255, 255, 255, 255),
    "expected pixel inside border to remain the element background"
  );
}
