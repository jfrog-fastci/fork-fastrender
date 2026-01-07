use super::util::{create_stacking_context_bounds_renderer, create_stacking_context_bounds_renderer_legacy};
use fastrender::Pixmap;

fn pixel(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
  let idx = (y as usize * pixmap.width() as usize + x as usize) * 4;
  let data = pixmap.data();
  [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
}

#[test]
fn border_image_outset_extends_stacking_context_bounds() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = create_stacking_context_bounds_renderer();
      let html = r#"
      <style>
        body { margin: 0; background: rgb(0, 0, 0); }
        #target {
          position: absolute;
          left: 40px;
          top: 40px;
          width: 20px;
          height: 20px;
          box-sizing: border-box;
          border: 2px solid transparent;
          border-image-source: linear-gradient(rgb(255, 0, 0), rgb(255, 0, 0));
          border-image-slice: 1;
          border-image-repeat: stretch;
          border-image-outset: 10px;
          isolation: isolate;
        }
      </style>
      <div id="target"></div>
      "#;

      let pixmap = renderer.render_html(html, 100, 100).expect("render");

      // The border box spans (40,40)-(60,60). The border image should paint with an outset of
      // 10px, reaching left to x=30. Sample just outside the border box but within that outset.
      let sample = pixel(&pixmap, 31, 50);
      assert!(
        sample[0] > 200 && sample[1] < 50 && sample[2] < 50 && sample[3] > 200,
        "expected border-image-outset pixels to be red-ish, got {sample:?}"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn border_image_outset_extends_stacking_context_bounds_legacy() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = create_stacking_context_bounds_renderer_legacy();
      let html = r#"
      <style>
        body { margin: 0; background: rgb(0, 0, 0); }
        #target {
          position: absolute;
          left: 40px;
          top: 40px;
          width: 20px;
          height: 20px;
          box-sizing: border-box;
          border: 2px solid transparent;
          border-image-source: linear-gradient(rgb(255, 0, 0), rgb(255, 0, 0));
          border-image-slice: 1;
          border-image-repeat: stretch;
          border-image-outset: 10px;
          isolation: isolate;
        }
      </style>
      <div id="target"></div>
      "#;

      let pixmap = renderer.render_html(html, 100, 100).expect("render");

      // The border box spans (40,40)-(60,60). The border image should paint with an outset of
      // 10px, reaching left to x=30. Sample just outside the border box but within that outset.
      let sample = pixel(&pixmap, 31, 50);
      assert!(
        sample[0] > 200 && sample[1] < 50 && sample[2] < 50 && sample[3] > 200,
        "expected border-image-outset pixels to be red-ish, got {sample:?}"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}
