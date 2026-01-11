use super::util::create_stacking_context_bounds_renderer;

#[test]
fn filter_blur_includes_offscreen_content_within_halo() {
  let mut renderer = create_stacking_context_bounds_renderer();

  // Regression test for filter kernel halo when culling to the viewport/clip rect.
  //
  // `#blob` is completely outside the clipped viewport (x < 0), but `#filtered` applies
  // `filter: blur(20px)`. The blur kernel should still sample the offscreen red pixels and
  // contribute to visible pixels inside the clip at x=0.
  //
  // Previously, the display-list renderer bounded the stacking-context layer to the parent clip
  // (and canvas) without any halo, so offscreen descendants were dropped and the blur produced no
  // visible red pixels.
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: black; }
      #clip {
        position: absolute;
        left: 0;
        top: 0;
        width: 100px;
        height: 100px;
        overflow: hidden;
      }
      #filtered {
        position: absolute;
        left: 0;
        top: 0;
        width: 100px;
        height: 100px;
        overflow: visible;
        filter: blur(20px);
      }
      #blob {
        position: absolute;
        /* Fully offscreen, but within the blur halo. */
        left: -50px;
        top: 40px;
        width: 40px;
        height: 20px;
        background: rgb(255, 0, 0);
      }
    </style>
    <div id="clip"><div id="filtered"><div id="blob"></div></div></div>
  "#;

  let pixmap = renderer
    .render_html(html, 100, 100)
    .expect("render should succeed");

  let p = pixmap.pixel(0, 50).expect("pixel inside viewport");
  assert!(
    p.red() > p.green() && p.red() > p.blue() && p.red() > 0,
    "expected blur() to sample offscreen red pixels, got rgba({}, {}, {}, {})",
    p.red(),
    p.green(),
    p.blue(),
    p.alpha()
  );
}

