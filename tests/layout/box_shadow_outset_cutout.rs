use fastrender::api::FastRender;

fn rgba_at(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let width = pixmap.width();
  let data = pixmap.data();
  let idx = (y * width + x) as usize * 4;
  (data[idx], data[idx + 1], data[idx + 2], data[idx + 3])
}

#[test]
fn outset_box_shadow_does_not_fill_box_interior() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      html, body { margin: 0; background: white; }
      #box {
        position: absolute;
        left: 50px;
        top: 20px;
        width: 30px;
        height: 30px;
        box-shadow: -2px 0 0 red;
      }
    </style>
    <div id="box"></div>
  "#;

  let pixmap = renderer.render_html(html, 100, 80).expect("render");

  // Shadow should only occupy the 2px strip immediately to the left of the box.
  let (r, g, b, a) = rgba_at(&pixmap, 49, 25);
  assert!(
    a > 0 && r > 200 && g < 80 && b < 80,
    "expected shadow pixel to be red-ish, got rgba=({r},{g},{b},{a})"
  );

  // The box has no background, so the interior should remain white.
  let (r, g, b, a) = rgba_at(&pixmap, 55, 25);
  assert!(
    a > 0 && r > 245 && g > 245 && b > 245,
    "expected box interior to stay white, got rgba=({r},{g},{b},{a})"
  );
}

