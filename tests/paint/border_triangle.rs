use super::util::{
  create_stacking_context_bounds_renderer, create_stacking_context_bounds_renderer_legacy,
};

fn assert_border_triangle_is_not_rect(pixmap: &tiny_skia::Pixmap) {
  // The triangle is formed by a 0×0 box with `border-top: 4px solid black` and
  // `border-left: 5px solid transparent`. With diagonal miter joins, the black area should occupy
  // only the right side (a triangle), not a full 5×4 rectangle.

  // Pick a point well inside the triangle (away from the diagonal edge) to avoid anti-alias
  // coverage differences.
  let inside = pixmap.pixel(4, 1).expect("inside pixel");
  assert!(
    inside.red() < 30 && inside.green() < 30 && inside.blue() < 30 && inside.alpha() > 200,
    "expected inside pixel to be black, got rgba({}, {}, {}, {})",
    inside.red(),
    inside.green(),
    inside.blue(),
    inside.alpha()
  );

  let outside = pixmap.pixel(1, 3).expect("outside pixel");
  assert!(
    outside.red() > 220 && outside.green() < 30 && outside.blue() < 30 && outside.alpha() > 200,
    "expected outside pixel to be background red, got rgba({}, {}, {}, {})",
    outside.red(),
    outside.green(),
    outside.blue(),
    outside.alpha()
  );
}

#[test]
fn solid_border_triangle_renders_as_triangle() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; background: #f00; }
      #tri {
        width: 0;
        height: 0;
        border-top: 4px solid #000;
        border-left: 5px solid transparent;
      }
    </style>
    <div id="tri"></div>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer.render_html(html, 10, 10).expect("render");
  assert_border_triangle_is_not_rect(&pixmap);

  let mut legacy = create_stacking_context_bounds_renderer_legacy();
  let pixmap = legacy.render_html(html, 10, 10).expect("render legacy");
  assert_border_triangle_is_not_rect(&pixmap);
}
