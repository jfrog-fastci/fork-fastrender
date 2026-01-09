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

fn assert_gap_vertical_rl(pixmap: &tiny_skia::Pixmap) {
  // With a 4px solid border, the right border stroke is centered at x=46:
  //   border box width = width (40) + left/right borders (4+4) = 48
  //   stroke center x = 48 - border_right/2 = 48 - 2 = 46
  let outside = pixmap.pixel(46, 190).expect("outside pixel");
  assert!(
    outside.red() < 30 && outside.green() < 30 && outside.blue() < 30 && outside.alpha() > 200,
    "expected border to paint outside legend gap, got rgba({}, {}, {}, {})",
    outside.red(),
    outside.green(),
    outside.blue(),
    outside.alpha()
  );

  let inside = pixmap.pixel(46, 30).expect("inside pixel");
  assert!(
    inside.red() > 220 && inside.green() > 220 && inside.blue() > 220 && inside.alpha() > 200,
    "expected border-right to be gapped behind legend, got rgba({}, {}, {}, {})",
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

#[test]
fn fieldset_border_does_not_paint_through_legend_vertical_rl() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; background: #fff; }
      fieldset {
        margin: 0;
        padding: 0;
        width: 40px;
        height: 200px;
        border: 4px solid #000;
        writing-mode: vertical-rl;
      }
      legend {
        margin: 0;
        padding: 40px 0;
        background: transparent;
      }
    </style>
    <fieldset>
      <legend>Legend</legend>
    </fieldset>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer.render_html(html, 80, 240).expect("render");
  assert_gap_vertical_rl(&pixmap);

  let mut legacy = create_stacking_context_bounds_renderer_legacy();
  let pixmap = legacy.render_html(html, 80, 240).expect("render legacy");
  assert_gap_vertical_rl(&pixmap);
}
