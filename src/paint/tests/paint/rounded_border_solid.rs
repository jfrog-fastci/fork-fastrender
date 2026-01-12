use super::util::{
  create_stacking_context_bounds_renderer, create_stacking_context_bounds_renderer_legacy,
};

fn assert_rounded_border_paints_corner_arc(pixmap: &tiny_skia::Pixmap) {
  let outside = pixmap.pixel(25, 25).expect("outside pixel");
  assert!(
    outside.red() > 220 && outside.green() < 30 && outside.blue() < 30 && outside.alpha() > 200,
    "expected outside pixel to be body background, got rgba({}, {}, {}, {})",
    outside.red(),
    outside.green(),
    outside.blue(),
    outside.alpha()
  );

  let left_edge = pixmap.pixel(2, 10).expect("left edge pixel");
  assert!(
    left_edge.red() < 30
      && left_edge.green() < 30
      && left_edge.blue() < 30
      && left_edge.alpha() > 200,
    "expected left edge border to be painted, got rgba({}, {}, {}, {})",
    left_edge.red(),
    left_edge.green(),
    left_edge.blue(),
    left_edge.alpha()
  );

  // Pixel inside the top-left corner arc region (between the outer and inner radii).
  let border_arc = pixmap.pixel(4, 4).expect("border arc pixel");
  assert!(
    border_arc.red() < 30
      && border_arc.green() < 30
      && border_arc.blue() < 30
      && border_arc.alpha() > 200,
    "expected rounded border arc to be painted, got rgba({}, {}, {}, {})",
    border_arc.red(),
    border_arc.green(),
    border_arc.blue(),
    border_arc.alpha()
  );

  let inside = pixmap.pixel(10, 10).expect("inside pixel");
  assert!(
    inside.red() < 30 && inside.green() > 220 && inside.blue() < 30 && inside.alpha() > 200,
    "expected inside to be element background, got rgba({}, {}, {}, {})",
    inside.red(),
    inside.green(),
    inside.blue(),
    inside.alpha()
  );
}

#[test]
fn solid_rounded_border_paints_corner_arcs() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; background: #f00; }
      #badge {
        box-sizing: border-box;
        width: 20px;
        height: 20px;
        border: 4px solid #000;
        border-radius: 10px;
        background: #0f0;
      }
    </style>
    <div id="badge"></div>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer.render_html(html, 30, 30).expect("render");
  assert_rounded_border_paints_corner_arc(&pixmap);

  let mut legacy = create_stacking_context_bounds_renderer_legacy();
  let pixmap = legacy.render_html(html, 30, 30).expect("render legacy");
  assert_rounded_border_paints_corner_arc(&pixmap);
}
