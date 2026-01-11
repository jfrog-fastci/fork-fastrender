use fastrender::FastRender;

fn pixel(pixmap: &resvg::tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
  let idx = (y as usize * pixmap.width() as usize + x as usize) * 4;
  let data = pixmap.data();
  [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
}

#[test]
fn text_wrap_pretty_does_not_rebalance() {
  // GitHub's marketing pages set `text-wrap: pretty` on a top-level wrapper. Chromium currently
  // treats `pretty` as a no-op, so we should keep greedy line-breaking behavior (no line
  // rebalancing) to match the baseline snapshots.
  //
  // This test ensures that `text-wrap: pretty` does not attempt to rebalance a 3+1 greedy wrap
  // into a more balanced 2+2 wrap.
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
        <style>
          body { margin: 0; background: white; }
          .container { width: 310px; text-wrap: pretty; font-size: 0; }
          .container span {
            display: inline-block;
            width: 100px;
            height: 40px;
            vertical-align: top;
          }
          #a { background: rgb(255, 0, 0); }
          #b { background: rgb(0, 255, 0); }
          #c { background: rgb(0, 0, 255); }
          #d { background: rgb(255, 255, 0); }
        </style>
        <div class="container">
          <span id="a"></span><wbr>
          <span id="b"></span><wbr>
          <span id="c"></span><wbr>
          <span id="d"></span>
        </div>
      "#;

      let pixmap = renderer.render_html(html, 320, 100).expect("render");
      assert_eq!(
        pixel(&pixmap, 210, 20),
        [0, 0, 255, 255],
        "expected the third inline block to remain on the first line under greedy wrapping"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

