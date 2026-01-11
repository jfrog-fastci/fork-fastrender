fn pixel(pixmap: &resvg::tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
  let idx = (y as usize * pixmap.width() as usize + x as usize) * 4;
  let data = pixmap.data();
  [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
}

#[test]
fn abspos_bottom_inset_uses_auto_height_containing_block() {
  // Regression test for nested absolute positioning where the containing block has `height:auto`.
  //
  // The absolutely positioned element (`.abs`) is *not* a direct child of the containing block
  // (`.cb`). Its containing block height is therefore only known after `.cb` lays out its in-flow
  // descendants. The renderer previously treated the containing block height as `0px` during
  // descendant layout, causing `bottom:0` to place the element above the containing block.
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = fastrender::FastRender::new().expect("renderer");
      let html = r#"
        <style>
          body { margin: 0; background: white; }
          .cb { position: relative; width: 100px; }
          .inner { height: 100px; background: rgb(0, 255, 0); }
          .abs { position: absolute; left: 0; bottom: 0; width: 100px; height: 20px; background: rgb(255, 0, 0); }
        </style>
        <div class="cb">
          <div class="inner">
            <div class="abs"></div>
          </div>
        </div>
      "#;

      let pixmap = renderer.render_html(html, 120, 120).expect("render");
      assert_eq!(
        pixel(&pixmap, 5, 95),
        [255, 0, 0, 255],
        "expected abspos element to sit at the bottom of its auto-height containing block"
      );
      assert_eq!(
        pixel(&pixmap, 5, 5),
        [0, 255, 0, 255],
        "expected the top of the containing block to remain green (no misplaced abspos overlay)"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn abspos_bottom_inset_uses_auto_height_containing_block_for_flex_item() {
  // Same regression as above, but exercise the code path where the containing block (`.cb`) is
  // laid out as a flex item (so its absolute-positioning containing block is established via the
  // block formatting context entrypoint, not `layout_block_child`).
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = fastrender::FastRender::new().expect("renderer");
      let html = r#"
        <style>
          body { margin: 0; background: white; }
          .flex { display: flex; align-items: flex-start; }
          .cb { position: relative; width: 100px; }
          .inner { height: 100px; background: rgb(0, 255, 0); }
          .abs { position: absolute; left: 0; bottom: 0; width: 100px; height: 20px; background: rgb(255, 0, 0); }
        </style>
        <div class="flex">
          <div class="cb">
            <div class="inner">
              <div class="abs"></div>
            </div>
          </div>
        </div>
      "#;

      let pixmap = renderer.render_html(html, 120, 120).expect("render");
      assert_eq!(
        pixel(&pixmap, 5, 95),
        [255, 0, 0, 255],
        "expected abspos element to sit at the bottom of its auto-height containing block"
      );
      assert_eq!(
        pixel(&pixmap, 5, 5),
        [0, 255, 0, 255],
        "expected the top of the containing block to remain green (no misplaced abspos overlay)"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}
