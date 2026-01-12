use super::util::create_stacking_context_bounds_renderer;

#[test]
fn stacking_context_layer_bounds_include_descendant_paint_overflow_forced_by_blend_isolation() {
  let mut renderer = create_stacking_context_bounds_renderer();

  // Regression fixture:
  // - `#outer` establishes a transform stacking context.
  // - `#blend` has a non-normal `mix-blend-mode`, which forces `#outer` to become `is_isolated=true`
  //   to correctly scope blending. This causes the display-list backend to allocate a bounded
  //   offscreen layer for `#outer`.
  // - `#shadow` paints a box-shadow that extends outside `#outer`'s border box.
  //
  // Without expanding the layer bounds to account for descendant paint overflow, the shadow will
  // be clipped to `#outer`'s bounds and the sampled pixel will remain background black.
  let html = r#"<!doctype html>
    <style>
      body { margin:0; background:black; }
      #outer {
        position:absolute;
        left:40px;
        top:40px;
        width:20px;
        height:20px;
        transform: translate(0px);
      }
      #shadow {
        width:20px;
        height:20px;
        background:blue;
        box-shadow: 0 0 0 10px rgb(255,0,0);
      }
      #blend {
        position:absolute;
        left:0;
        top:0;
        width:1px;
        height:1px;
        background: rgb(0,255,0);
        mix-blend-mode: multiply;
      }
    </style>
    <div id="outer"><div id="shadow"></div><div id="blend"></div></div>
  "#;

  let pixmap = renderer
    .render_html(html, 100, 100)
    .expect("render should succeed");

  // Sample a pixel that is outside `#outer`'s border box (x < 40) but within `#shadow`'s shadow
  // region (shadow extends to x=30..70).
  let p = pixmap.pixel(32, 50).expect("pixel inside viewport");
  assert!(
    p.red() > p.green() && p.red() > p.blue() && p.red() > 0,
    "expected descendant shadow to be visible outside stacking-context bounds, got rgba({}, {}, {}, {})",
    p.red(),
    p.green(),
    p.blue(),
    p.alpha()
  );
}
