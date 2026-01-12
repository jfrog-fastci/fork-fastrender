use super::util::create_stacking_context_bounds_renderer;

#[test]
fn svg_img_preserve_aspect_ratio_letterboxes_by_default() {
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: white; }
      img { display: block; width: 100px; height: 100px; }
    </style>
    <img src="data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 200 100'><rect width='200' height='100' fill='red'/></svg>" />
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer.render_html(html, 100, 100).expect("render");

  // 2:1 viewBox in a 1:1 viewport => `preserveAspectRatio="xMidYMid meet"` letterboxes vertically.
  let top = pixmap.pixel(50, 10).expect("top pixel");
  assert_eq!(
    (top.red(), top.green(), top.blue(), top.alpha()),
    (255, 255, 255, 255),
    "expected SVG content to be vertically letterboxed"
  );

  let center = pixmap.pixel(50, 50).expect("center pixel");
  assert_eq!(
    (center.red(), center.green(), center.blue(), center.alpha()),
    (255, 0, 0, 255),
    "expected SVG content to render within the centered viewport"
  );
}
