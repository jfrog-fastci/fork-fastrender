use super::util::create_stacking_context_bounds_renderer;

#[test]
fn hr_default_border_has_no_inner_gap() {
  // `hr` uses an inset border in the UA stylesheet. The element's default `height` should be `0`
  // so the top/bottom border edges touch (2px total border-box height) rather than leaving a
  // 1px "content gap" between them.
  //
  // Repro (prior bug):
  // - UA stylesheet used `height: 1px` along with `border: 1px inset ...`.
  // - That produced a 3px-tall rule (border + 1px content + border) with a visible gap on pages
  //   like openbsd.org.
  let html = r#"
    <style>
      html, body { margin: 0; background: #fff; }
      /* Keep the rule at the top edge so pixel sampling is stable. */
      hr { margin: 0; }
    </style>
    <hr>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer.render_html(html, 20, 6).expect("render");

  let top = pixmap.pixel(10, 0).expect("top pixel");
  let bottom = pixmap.pixel(10, 1).expect("bottom pixel");
  let after = pixmap.pixel(10, 2).expect("background pixel below hr");

  assert_eq!(
    (top.red(), top.green(), top.blue(), top.alpha()),
    (144, 144, 144, 255),
    "expected the top inset border edge to be the darker shade"
  );
  assert_eq!(
    (bottom.red(), bottom.green(), bottom.blue(), bottom.alpha()),
    (240, 240, 240, 255),
    "expected the bottom inset border edge to be the lighter shade (no inner gap)"
  );
  assert_eq!(
    (after.red(), after.green(), after.blue(), after.alpha()),
    (255, 255, 255, 255),
    "expected the pixel below the rule to be background, confirming hr is 2px tall"
  );
}
