use super::util::create_stacking_context_bounds_renderer;

#[test]
fn stacking_context_layer_bounds_include_descendant_filter_outset() {
  let mut renderer = create_stacking_context_bounds_renderer();

  // Regression fixture:
  // - `#outer` establishes a stacking context and therefore a bounded compositing layer.
  // - `#inner` applies a `filter: drop-shadow(...)` that renders outside `#outer`'s border box.
  //
  // Without accounting for descendant filter outsets when computing the stacking context bounds,
  // the shadow pixels will be clipped by `#outer`'s bounded layer.
  let html = r#"<!doctype html>
    <style>
      body { margin:0; background:black; }
      #outer {
        isolation:isolate;
        position:absolute;
        left:40px;
        top:40px;
        width:20px;
        height:20px;
      }
      #inner {
        width:20px;
        height:20px;
        background:blue;
        filter: drop-shadow(-10px 0 0 rgb(255 0 0));
      }
    </style>
    <div id="outer"><div id="inner"></div></div>
  "#;

  let pixmap = renderer
    .render_html(html, 100, 100)
    .expect("render should succeed");

  // Sample a pixel outside `#outer`'s border box (x < 40) but within the drop-shadow region
  // (shadow covers x=30..50).
  let p = pixmap.pixel(32, 50).expect("pixel inside viewport");
  assert!(
    p.red() > p.green() && p.red() > p.blue() && p.red() > 0,
    "expected descendant filter shadow to be visible outside stacking-context bounds, got rgba({}, {}, {}, {})",
    p.red(),
    p.green(),
    p.blue(),
    p.alpha()
  );
}

