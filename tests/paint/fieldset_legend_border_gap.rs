use super::util::{create_stacking_context_bounds_renderer, create_stacking_context_bounds_renderer_legacy};

fn assert_gap(pixmap: &tiny_skia::Pixmap) {
  // With a 4px solid border, the border stroke is centered at y=2.
  let outside = pixmap.pixel(180, 2).expect("outside pixel");
  assert!(
    outside.red() < 30 && outside.green() < 30 && outside.blue() < 30 && outside.alpha() > 200,
    "expected border to paint outside legend gap, got rgba({}, {}, {}, {})",
    outside.red(),
    outside.green(),
    outside.blue(),
    outside.alpha()
  );

  let inside = pixmap.pixel(30, 2).expect("inside pixel");
  assert!(
    inside.red() > 220 && inside.green() > 220 && inside.blue() > 220 && inside.alpha() > 200,
    "expected border-top to be gapped behind legend, got rgba({}, {}, {}, {})",
    inside.red(),
    inside.green(),
    inside.blue(),
    inside.alpha()
  );
}

#[test]
fn fieldset_border_does_not_paint_through_legend() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; background: #fff; }
      fieldset {
        margin: 0;
        padding: 0;
        width: 200px;
        height: 40px;
        border: 4px solid #000;
      }
      legend {
        margin: 0;
        padding: 0 40px;
        background: transparent;
      }
    </style>
    <fieldset>
      <legend>Legend</legend>
    </fieldset>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer.render_html(html, 240, 80).expect("render");
  assert_gap(&pixmap);

  let mut legacy = create_stacking_context_bounds_renderer_legacy();
  let pixmap = legacy.render_html(html, 240, 80).expect("render legacy");
  assert_gap(&pixmap);
}

