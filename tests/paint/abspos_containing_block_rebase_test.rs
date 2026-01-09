use fastrender::FastRender;

fn pixel(pixmap: &resvg::tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
  let idx = (y as usize * pixmap.width() as usize + x as usize) * 4;
  let data = pixmap.data();
  [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
}

#[test]
fn abspos_descendant_does_not_double_apply_grid_item_offset() {
  // Regression test: when a grid (or flex) item is positioned at a non-zero origin, nested
  // formatting contexts must translate the inherited positioned/fixed containing blocks into the
  // child's coordinate space.
  //
  // Without that translation, an absolutely positioned descendant whose containing block is an
  // ancestor can be laid out in the ancestor's coordinate space but then painted relative to the
  // grid item, effectively adding the grid item's offset twice.
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      body { margin: 0; background: white; }

      .cb {
        position: relative;
        display: grid;
        grid-template-columns: 200px 200px;
        width: 400px;
        height: 60px;
      }

      .abs {
        position: absolute;
        left: -10px;
        top: 0;
        width: 50px;
        height: 50px;
        background: rgb(61, 187, 219);
        transform: translateX(-100%);
      }
    </style>

    <div class="cb">
      <div></div>
      <div>
        <div>
          <div class="abs"></div>
        </div>
      </div>
    </div>
  "#;

  let pixmap = renderer.render_html(html, 400, 60).expect("render");

  // The box is shifted fully offscreen to the left. If the grid item offset is accidentally added
  // twice, the turquoise box appears in the second column (x ≈ 140..190).
  assert_eq!(pixel(&pixmap, 160, 25), [255, 255, 255, 255]);
}

