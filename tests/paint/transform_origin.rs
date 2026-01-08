use fastrender::FastRender;

fn pixel(pixmap: &resvg::tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
  let idx = (y as usize * pixmap.width() as usize + x as usize) * 4;
  let data = pixmap.data();
  [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
}

#[test]
fn transform_origin_affects_painting() {
  // `transform-origin` changes the pivot point used to compute the element's transform matrix.
  //
  // MDN uses small rotated chevrons/indicators in the UI chrome; if we ignore transform-origin (or
  // default to the wrong initial value), those icons drift away from their intended positions.
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      .box {
        position: absolute;
        width: 40px;
        height: 20px;
      }
      /* Default transform-origin is center (50% 50%). */
      #default {
        left: 20px;
        top: 20px;
        background: rgb(255, 0, 0);
        transform: rotate(90deg);
      }
      /* Override origin to top-left corner. */
      #top-left {
        left: 120px;
        top: 20px;
        background: rgb(0, 0, 255);
        transform-origin: 0 0;
        transform: rotate(90deg);
      }
    </style>
    <div id="default" class="box"></div>
    <div id="top-left" class="box"></div>
  "#;

  let pixmap = renderer.render_html(html, 200, 100).expect("render");

  // The red element rotates around its center and should stay near its original location.
  assert_eq!(pixel(&pixmap, 40, 30), [255, 0, 0, 255]);
  // If we incorrectly rotate around top-left, the red box would shift left and cover (10, 40).
  assert_eq!(pixel(&pixmap, 10, 40), [255, 255, 255, 255]);

  // The blue element rotates around its top-left corner and shifts left, so (110, 40) is inside.
  assert_eq!(pixel(&pixmap, 110, 40), [0, 0, 255, 255]);
  // If we ignored `transform-origin`, the blue box would still be centered around x≈140.
  assert_eq!(pixel(&pixmap, 140, 30), [255, 255, 255, 255]);
}

