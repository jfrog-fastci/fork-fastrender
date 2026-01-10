use fastrender::FastRender;

fn pixel(pixmap: &resvg::tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
  let idx = (y as usize * pixmap.width() as usize + x as usize) * 4;
  let data = pixmap.data();
  [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
}

#[test]
fn html_body_height_100_percent_enables_percent_height_children() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      // CSS2.1 §10.5: Percentage heights resolve against the containing block's *definite* height.
      //
      // The initial containing block (viewport) provides a definite percentage base for the root
      // element, enabling the common `html, body { height: 100% }` pattern used by many pageset
      // targets.
      //
      // Regression: The layout engine did not set the initial containing block height as the block
      // percentage base, so `height:100%` on the `<html>` element computed to `auto`, causing
      // descendants with `height:100%` to collapse to 0px.
      let html = r#"
        <!doctype html>
        <html>
          <head>
            <style>
              html, body { margin: 0; height: 100%; }
              #fill { height: 100%; background: rgb(255, 0, 0); }
            </style>
          </head>
          <body>
            <div id="fill"></div>
          </body>
        </html>
      "#;

      let pixmap = renderer.render_html(html, 64, 64).expect("render");

      assert_eq!(
        pixel(&pixmap, 32, 63),
        [255, 0, 0, 255],
        "expected `height:100%` child to fill the viewport when `html, body` are 100%"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

