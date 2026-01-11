use fastrender::FastRender;

#[test]
fn abspos_inset_zero_inside_padded_container_does_not_bleed_into_previous_sibling() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      body { margin: 0; background: white; }

      .header {
        height: 80px;
        background: rgb(255, 0, 0);
      }

      .container {
        position: relative;
        padding: 64px 0 0 32px;
        background: white;
      }

      .container::before {
        content: "";
        position: absolute;
        inset: 0;
        background: rgba(0, 0, 0, 0.5);
      }
    </style>

    <div class="header"></div>
    <div class="container"></div>
  "#;

  let pixmap = renderer.render_html(html, 200, 120).expect("render");

  let pixel = pixmap.pixel(10, 40).expect("pixel in bounds");
  assert_eq!(
    (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
    (255, 0, 0, 255)
  );
}

