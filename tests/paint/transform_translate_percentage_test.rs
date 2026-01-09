use fastrender::FastRender;

fn pixel(pixmap: &resvg::tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
  let idx = (y as usize * pixmap.width() as usize + x as usize) * 4;
  let data = pixmap.data();
  [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
}

#[test]
fn transform_translate_percentage_hides_box_offscreen() {
  // Regression test for `transform: translateX(-100%)`.
  //
  // This pattern is common for skip-links and drawer menus: place the element slightly offscreen
  // using `left: <negative>` and then translate it by its own width via a percentage transform.
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      #box {
        position: absolute;
        left: -10px;
        top: 0;
        width: 100px;
        height: 50px;
        background: rgb(61, 187, 219);
        transform: translateX(-100%);
      }
    </style>
    <div id="box"></div>
  "#;

  let pixmap = renderer.render_html(html, 200, 60).expect("render");

  // Box should be entirely offscreen to the left; the viewport remains the body background.
  assert_eq!(pixel(&pixmap, 0, 25), [255, 255, 255, 255]);
  assert_eq!(pixel(&pixmap, 50, 25), [255, 255, 255, 255]);
}

