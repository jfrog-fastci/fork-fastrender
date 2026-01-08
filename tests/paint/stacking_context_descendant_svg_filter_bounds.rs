use super::util::create_stacking_context_bounds_renderer;

#[test]
fn stacking_context_layer_bounds_include_descendant_svg_filter_outset() {
  let mut renderer = create_stacking_context_bounds_renderer();

  // Regression fixture:
  // - `#outer` establishes a stacking context and therefore a bounded compositing layer.
  // - `#inner` applies a `filter: url(#blur)` SVG filter whose output extends outside `#outer`'s
  //   border box.
  //
  // Without accounting for descendant SVG filter outsets when computing stacking context bounds,
  // the blurred pixels will be clipped by `#outer`'s bounded layer.
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
        background:rgb(255 0 0);
        filter:url(#blur);
      }
      svg { position:absolute; width:0; height:0; }
    </style>
    <svg width="0" height="0" aria-hidden="true">
      <filter id="blur" x="-50%" y="-50%" width="200%" height="200%">
        <feGaussianBlur stdDeviation="4"/>
      </filter>
    </svg>
    <div id="outer"><div id="inner"></div></div>
  "#;

  let pixmap = renderer
    .render_html(html, 100, 100)
    .expect("render should succeed");

  // Sample a pixel outside `#outer`'s border box (x < 40) but within the blur region.
  let p = pixmap.pixel(37, 50).expect("pixel inside viewport");
  assert!(
    p.red() > p.green() && p.red() > p.blue() && p.red() > 0,
    "expected descendant SVG filter blur to be visible outside stacking-context bounds, got rgba({}, {}, {}, {})",
    p.red(),
    p.green(),
    p.blue(),
    p.alpha()
  );
}

