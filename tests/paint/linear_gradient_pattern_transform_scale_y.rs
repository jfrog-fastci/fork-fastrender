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

#[test]
fn linear_gradient_pattern_does_not_bleed_outside_dest_rect_at_fractional_edges() {
  // Regression for python.org: the main header background uses a repeating linear-gradient pattern
  // and starts at a fractional device pixel. The pattern renderer must not expand the painted
  // region to the `floor/ceil` bounds of the destination rect, otherwise it overwrites the scanline
  // immediately above the element (hiding the preceding section's 1px border).
  let html = r#"
    <style>
      body { margin: 0; background: rgb(31, 59, 71); }
      #target {
        position: absolute;
        left: 0;
        top: 120.944534px;
        width: 100px;
        height: 50px;
        background-color: rgb(43, 91, 132);
        /* Default background-repeat is `repeat`, which exercises the Pattern display item path. */
        background-image: linear-gradient(180deg, rgb(30, 65, 94) 10%, rgb(43, 91, 132) 90%);
      }
    </style>
    <div id="target"></div>
  "#;

  let pixmap = render_display_list(html, 120, 200);

  // Pixel y=120 is above the element's top edge (120.9445px), so it must remain the body
  // background color.
  let (r, g, b, a) = rgba_at(&pixmap, 0, 120);
  assert_eq!(
    (r, g, b, a),
    (31, 59, 71, 255),
    "expected background above the element to remain unchanged, got rgba=({r},{g},{b},{a})"
  );
}

#[test]
fn linear_gradient_pattern_does_not_bleed_past_fractional_bottom_edge() {
  // Regression for python.org: the active meta-navigation tab has an opaque linear-gradient
  // background whose bottom edge lands on a fractional pixel. The gradient renderer must not
  // expand the painted region to `ceil(height)` pixels and overwrite the 1px border immediately
  // below the element.
  let html = r#"
    <style>
      body { margin: 0; background: rgb(31, 59, 71); }
      #target {
        position: absolute;
        left: 0;
        top: 77.544534px;
        width: 100px;
        height: 42.4px;
        background-color: rgb(31, 42, 50);
        /* Default background-repeat is `repeat`, which exercises the Pattern display item path. */
        background-image: linear-gradient(180deg, rgb(19, 25, 30) 10%, rgb(31, 42, 50) 90%);
      }
    </style>
    <div id="target"></div>
  "#;

  let pixmap = render_display_list(html, 120, 160);

  // Pixel y=120 is below the element's bottom edge (119.9445px), so it must remain the body
  // background color.
  let (r, g, b, a) = rgba_at(&pixmap, 0, 120);
  assert_eq!(
    (r, g, b, a),
    (31, 59, 71, 255),
    "expected background below the element to remain unchanged, got rgba=({r},{g},{b},{a})"
  );
}
