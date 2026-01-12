use crate::FastRender;

fn pixel(pixmap: &resvg::tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
  let idx = (y as usize * pixmap.width() as usize + x as usize) * 4;
  let data = pixmap.data();
  [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
}

#[test]
fn overflow_hidden_clips_transformed_descendant() {
  // The canvas clip fast path can represent rectangular clips using only bounds. That is
  // sufficient for axis-aligned drawing, but transformed descendants still need the clip to be
  // enforced.
  //
  // Regression: ensure `overflow: hidden` clips descendants with `transform`.
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      .clip {
        position: absolute;
        left: 20px;
        top: 40px;
        width: 60px;
        height: 60px;
        overflow: hidden;
      }
      .child {
        width: 60px;
        height: 60px;
        background: rgb(0, 0, 255);
        transform: translateY(-30px);
      }
    </style>
    <div class="clip"><div class="child"></div></div>
  "#;

  let pixmap = renderer.render_html(html, 120, 120).expect("render");

  // This pixel is above the clip box but inside the transformed blue child; it should remain
  // the page background (white).
  assert_eq!(pixel(&pixmap, 30, 20), [255, 255, 255, 255]);
  // This pixel lies within the clip box and should be painted (blue).
  assert_eq!(pixel(&pixmap, 30, 50), [0, 0, 255, 255]);
}
