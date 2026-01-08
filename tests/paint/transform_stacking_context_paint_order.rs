use fastrender::FastRender;

fn pixel(pixmap: &resvg::tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
  let idx = (y as usize * pixmap.width() as usize + x as usize) * 4;
  let data = pixmap.data();
  [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
}

#[test]
fn transform_stacking_context_paints_above_in_flow_blocks() {
  // CSS Transforms: an element with `transform != none` creates a stacking context and is
  // painted as if it were a positioned element with z-index:auto (i.e. in layer 6).
  //
  // This means it should paint above in-flow block descendants, even if it appears earlier
  // in the DOM.
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      /* Creates a stacking context without visually moving the element. */
      .transformed { transform: translateX(0px); width: 100px; height: 100px; background: rgb(255, 0, 0); }
      .normal { width: 100px; height: 100px; margin-top: -50px; background: rgb(0, 0, 255); }
    </style>
    <div class="transformed"></div>
    <div class="normal"></div>
  "#;

  let pixmap = renderer.render_html(html, 100, 160).expect("render");

  // Overlap region is y=50..100. The transformed (red) element should paint above the normal
  // (blue) element.
  assert_eq!(pixel(&pixmap, 50, 75), [255, 0, 0, 255]);
  // Region only covered by the second element (y=100..150) should remain blue.
  assert_eq!(pixel(&pixmap, 50, 125), [0, 0, 255, 255]);
}

