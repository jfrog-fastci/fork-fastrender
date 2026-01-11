use super::util::create_stacking_context_bounds_renderer;

fn rgba_at(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
  let p = pixmap.pixel(x, y).expect("pixel");
  [p.red(), p.green(), p.blue(), p.alpha()]
}

#[test]
fn filter_source_is_not_clipped_by_overflow_hidden_ancestor() {
  let mut renderer = create_stacking_context_bounds_renderer();

  // Regression test: ancestor overflow clips should be applied *after* `filter` effects.
  //
  // If we inherit the ancestor clip while painting into the filter's offscreen layer, we clip the
  // source graphic before filtering. That changes the filter result near the clip edge (e.g. blur
  // incorrectly fades toward the backdrop even when the source extends outside the clip).
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      .clip {
        position: absolute;
        left: 0px;
        top: 50px;
        width: 100px;
        height: 100px;
        overflow: hidden;
      }
      .blur {
        position: absolute;
        left: 0px;
        top: -40px;
        width: 100px;
        height: 100px;
        background: rgb(0, 0, 255);
        filter: blur(10px);
      }
    </style>
    <div class="clip"><div class="blur"></div></div>
  "#;

  let pixmap = renderer.render_html(html, 200, 200).expect("render");

  // Above the clipped area: just the page background.
  assert_eq!(rgba_at(&pixmap, 50, 49), [255, 255, 255, 255]);

  // At the top edge of the clip: the filtered result should remain solid blue because the source
  // graphic continues above the clip edge. (If the source were clipped before filtering, blur would
  // fade toward white here.)
  let edge = rgba_at(&pixmap, 50, 50);
  assert!(
    edge[0] < 10 && edge[1] < 10 && edge[2] > 200,
    "expected the blur to stay blue at the clip edge, got {:?}",
    edge
  );
}

