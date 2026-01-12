use super::util::create_stacking_context_bounds_renderer;
use tiny_skia::Pixmap;

fn render(html: &str, width: u32, height: u32) -> Pixmap {
  let mut renderer = create_stacking_context_bounds_renderer();
  renderer.render_html(html, width, height).expect("render")
}

fn color_at(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
  let pixel = pixmap.pixel(x, y).expect("pixel");
  [pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()]
}

#[test]
fn stacking_context_layer_bounds_include_box_shadow_overflow() {
  let html = r#"
    <style>
      body { margin: 0; background: black; }
      #target {
        position: absolute;
        left: 40px;
        top: 40px;
        width: 20px;
        height: 20px;
        background: rgb(0, 0, 255);
        box-shadow: 0 0 0 10px rgb(255, 0, 0);
        isolation: isolate;
      }
    </style>
    <div id="target"></div>
  "#;

  let pixmap = render(html, 100, 100);

  let outside = color_at(&pixmap, 32, 50);
  assert!(
    outside[0] > outside[1] && outside[0] > outside[2] && outside[0] > 0,
    "expected box-shadow pixels outside the border box to remain visible, got {:?}",
    outside
  );
}
