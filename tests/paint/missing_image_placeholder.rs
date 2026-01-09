use fastrender::FastRender;

fn pixel_rgba(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

#[test]
fn missing_image_placeholder_does_not_flood_fill_replaced_box() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      body { margin: 0; }
      .bg { width: 100px; height: 100px; background: rgb(0, 255, 0); }
      img { display: block; width: 100px; height: 100px; }
    </style>
    <div class="bg"><img src="data:image/png;base64,"></div>
  "#;

  let pixmap = renderer.render_html(html, 100, 100).expect("render");
  let (r, g, b, _) = pixel_rgba(&pixmap, 50, 50);
  assert!(
    g > r + 80 && g > b + 80 && g > 80,
    "expected missing image to keep background visible (r={r}, g={g}, b={b})"
  );
}

