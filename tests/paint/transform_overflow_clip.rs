use fastrender::FastRender;

fn pixel(pixmap: &resvg::tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
  let idx = (y as usize * pixmap.width() as usize + x as usize) * 4;
  let data = pixmap.data();
  [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
}

#[test]
fn overflow_clip_is_transformed_with_stacking_context() {
  // When an element with `overflow: hidden` is transformed, the clip region is part of the
  // element's rendering and therefore participates in the element's transform.
  //
  // This regression checks that we apply the overflow clip *after* the stacking context
  // transform is set on the canvas so the clip is rotated along with its contents.
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      .clip {
        width: 60px;
        height: 60px;
        margin: 20px;
        overflow: hidden;
        transform: rotate(45deg);
      }
      .child {
        width: 200px;
        height: 200px;
        background: rgb(0, 0, 255);
      }
    </style>
    <div class="clip"><div class="child"></div></div>
  "#;

  let pixmap = renderer.render_html(html, 120, 120).expect("render");

  // The transformed clip is a diamond centered at (50, 50). Pixel (25, 25) is well outside the
  // diamond and should remain the page background (white).
  assert_eq!(pixel(&pixmap, 25, 25), [255, 255, 255, 255]);
  // Pixel (50, 25) is inside the diamond and should be clipped in (blue).
  assert_eq!(pixel(&pixmap, 50, 25), [0, 0, 255, 255]);
}

