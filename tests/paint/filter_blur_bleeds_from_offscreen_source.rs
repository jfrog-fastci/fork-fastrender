use super::util::create_stacking_context_bounds_renderer;

#[test]
fn filter_blur_bleeds_from_offscreen_source() {
  let mut renderer = create_stacking_context_bounds_renderer();

  // Regression fixture:
  // - `#filter` establishes a filtered stacking context that is *partially visible* (so it won't be
  //   culled before painting).
  // - `#source` is an offscreen descendant whose pixels should still contribute to the blur halo
  //   that lands inside the viewport.
  //
  // If the filter input surface is clipped to the viewport before evaluating the blur kernel, the
  // offscreen source pixels will be dropped and the halo will be missing.
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: black; }
      #filter {
        position: absolute;
        left: 0px;
        top: 40px;
        width: 1px;
        height: 1px;
        filter: blur(10px);
      }
      #source {
        position: absolute;
        left: -30px;
        top: 0px;
        width: 20px;
        height: 20px;
        background: rgb(255 0 0);
      }
    </style>
    <div id="filter"><div id="source"></div></div>
  "#;

  let pixmap = renderer
    .render_html(html, 100, 100)
    .expect("render should succeed");

  // The box is offscreen, so any non-black pixel here must come from the blur halo.
  let p = pixmap.pixel(0, 50).expect("pixel inside viewport");
  assert!(
    p.red() > p.green() && p.red() > p.blue() && p.red() > 0,
    "expected blur halo from offscreen source, got rgba({}, {}, {}, {})",
    p.red(),
    p.green(),
    p.blue(),
    p.alpha()
  );
}
