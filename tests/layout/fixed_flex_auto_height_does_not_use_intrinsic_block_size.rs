use fastrender::FastRender;

fn pixel(pixmap: &resvg::tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
  let idx = (y as usize * pixmap.width() as usize + x as usize) * 4;
  let data = pixmap.data();
  [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
}

#[test]
fn fixed_flex_auto_height_does_not_use_intrinsic_block_size() {
  // Regression test for positioned layout:
  //
  // When `top/bottom/height` are all `auto`, CSS2.1 sizes the out-of-flow box based on its
  // normal-flow (content) height. We used to compute and feed a max-content intrinsic block size
  // into the abspos sizing algorithm, which could overshoot the real content height when the
  // intrinsic probe ran under an intrinsic inline constraint. For flex containers with
  // `justify-content: center`, that produced extra free space and shifted the children down.
  //
  // GitHub's home page depends on this behaving like Chromium (no extra vertical centering offset).
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
        <style>
          body { margin: 0; background: white; }
          .spacer { height: 20px; }
          #fixed {
            position: fixed;
            display: flex;
            flex-direction: column;
            justify-content: center;
            width: 100%;
          }
          #marker {
            background: rgb(255, 0, 0);
            font: 20px/20px monospace;
            width: 100%;
          }
        </style>
        <div class="spacer"></div>
        <div id="fixed">
          <div id="marker">
            A A A A A A A A A A A A A A A A A A A A A A A A A A A A A A A A A
          </div>
        </div>
      "#;

      let pixmap = renderer.render_html(html, 300, 200).expect("render");
      assert_eq!(
        pixel(&pixmap, 1, 25),
        [255, 0, 0, 255],
        "expected marker background to begin at the fixed element's static position (no justify-content offset)"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}
