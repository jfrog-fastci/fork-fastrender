use super::util::create_stacking_context_bounds_renderer;

#[test]
fn filter_blur_samples_outside_ancestor_clip() {
  let mut renderer = create_stacking_context_bounds_renderer();

  // Regression fixture:
  // - `#clip` clips its children via `overflow: hidden`.
  // - `#filtered` applies a blur filter.
  // - `#bar` sits fully outside `#clip` (x < 0) but within the blur kernel distance.
  //
  // The blur should still include `#bar` when computing pixels inside `#clip`, because the clip is
  // applied to the filtered output, not to the source graphic used as filter input.
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: white; }
      #clip {
        width: 100px;
        height: 100px;
        overflow: hidden;
        background: white;
      }
      #filtered {
        position: relative;
        width: 100px;
        height: 100px;
        filter: blur(10px);
      }
      #bar {
        position: absolute;
        left: -30px;
        top: 0;
        width: 30px;
        height: 100px;
        background: black;
      }
    </style>
    <div id="clip"><div id="filtered"><div id="bar"></div></div></div>
  "#;

  let pixmap = renderer
    .render_html(html, 100, 100)
    .expect("render should succeed");

  let px = pixmap.pixel(2, 50).expect("pixel in bounds");
  assert!(
    px.red() < 250 && px.green() < 250 && px.blue() < 250,
    "expected blur to sample pixels outside ancestor clip, got rgba({}, {}, {}, {})",
    px.red(),
    px.green(),
    px.blue(),
    px.alpha()
  );
}

