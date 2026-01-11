use super::util::create_stacking_context_bounds_renderer;
use tiny_skia::Pixmap;

fn render_display_list(html: &str, width: u32, height: u32) -> Pixmap {
  let mut renderer = create_stacking_context_bounds_renderer();
  renderer.render_html(html, width, height).expect("render")
}

fn rgba_at(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).expect("pixel");
  (p.red(), p.green(), p.blue(), p.alpha())
}

#[test]
fn linear_gradient_pattern_respects_stacking_context_scale_transform() {
  // Regression for linear-gradient pattern painting under stacking-context transforms.
  //
  // The gradient is constant white until 39.9% of the element height. With `scaleY(2.5)` the
  // constant-white region should extend to ~80px in device space. If the transform is ignored or
  // only applied to the destination rect but not to gradient sampling, pixels around y=70 become
  // noticeably gray.
  let html = r#"
    <style>
      body { margin: 0; background: rgb(0, 0, 0); }
      #target {
        position: absolute;
        top: 0;
        left: 0;
        width: 100px;
        height: 80px;
        transform-origin: 0 0;
        transform: scaleY(2.5);
        /* Default background-repeat is `repeat`, which exercises the Pattern display item path. */
        background-image: linear-gradient(180deg, rgb(255, 255, 255) 39.9%, rgb(248, 248, 248) 100%);
      }
    </style>
    <div id="target"></div>
  "#;

  let pixmap = render_display_list(html, 120, 220);

  let (r, g, b, a) = rgba_at(&pixmap, 50, 70);
  assert_eq!(
    (r, g, b, a),
    (255, 255, 255, 255),
    "expected constant-white region under scaleY transform, got rgba=({r},{g},{b},{a})"
  );
}

#[test]
fn linear_gradient_pattern_with_rounded_clip_respects_scale_transform() {
  // Same regression as above, but with a rounded clip (border-radius + overflow hidden) to ensure
  // the pattern path is correct when the canvas has a clip mask.
  let html = r#"
    <style>
      body { margin: 0; background: rgb(0, 0, 0); }
      #target {
        position: absolute;
        top: 0;
        left: 0;
        width: 200px;
        height: 80px;
        border-radius: 40px;
        overflow: hidden;
        transform-origin: 0 0;
        transform: scaleY(2.5);
        background-image: linear-gradient(180deg, rgb(255, 255, 255) 39.9%, rgb(248, 248, 248) 100%);
      }
    </style>
    <div id="target"></div>
  "#;

  let pixmap = render_display_list(html, 220, 220);
  let (r, g, b, a) = rgba_at(&pixmap, 100, 70);
  assert_eq!(
    (r, g, b, a),
    (255, 255, 255, 255),
    "expected constant-white region under scaleY transform (with rounded clip), got rgba=({r},{g},{b},{a})"
  );
}
