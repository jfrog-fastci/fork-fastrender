use fastrender::FastRender;

fn rgba_at(pixmap: &fastrender::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).expect("pixel in bounds");
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn quirks_body_auto_height_provides_percent_height_base_for_descendants() {
  crate::common::with_large_stack(|| {
    let mut renderer = FastRender::new().expect("renderer");
    // In quirks mode, browsers treat the `<body>` element as having a definite block-size for the
    // purpose of percentage resolution. This enables legacy patterns like `.container { height:
    // 100% }` even without `html, body { height: 100% }`.
    //
    // Regression: FastRender resolved `%` heights against an indefinite base in quirks mode,
    // causing `height: 100%` to compute to `auto` and collapse to its content size.
    let html = r#"
      <html>
        <head>
          <style>
            body { margin: 8px; background: rgb(255, 255, 255); }
            #fill { height: 100%; background: rgb(255, 0, 0); }
          </style>
        </head>
        <body>
          <div id="fill"></div>
        </body>
      </html>
    "#;

    let pixmap = renderer.render_html(html, 64, 64).expect("render");

    // The red element should fill the body content box (64px viewport - 8px top/bottom margins).
    assert_eq!(rgba_at(&pixmap, 8, 8), (255, 0, 0, 255));
    assert_eq!(
      rgba_at(&pixmap, 8, 55),
      (255, 0, 0, 255),
      "expected `height:100%` to fill the viewport excluding body margins"
    );
    assert_eq!(
      rgba_at(&pixmap, 8, 63),
      (255, 255, 255, 255),
      "expected body margins to remain unpainted by the `height:100%` child"
    );
  });
}

#[test]
fn quirks_percent_height_enables_flex_vertical_centering() {
  crate::common::with_large_stack(|| {
    let mut renderer = FastRender::new().expect("renderer");
    let html = r#"
      <html>
        <head>
          <style>
            body { margin: 8px; background: rgb(255, 255, 255); }
            .container {
              display: flex;
              height: 100%;
              justify-content: center;
              align-items: center;
            }
            #box { width: 10px; height: 10px; background: rgb(0, 0, 255); }
          </style>
        </head>
        <body>
          <div class="container">
            <div id="box"></div>
          </div>
        </body>
      </html>
    "#;

    let pixmap = renderer.render_html(html, 100, 100).expect("render");

    assert_eq!(
      rgba_at(&pixmap, 50, 50),
      (0, 0, 255, 255),
      "expected the 10x10 box to be vertically centered (quirks-mode `%` height must resolve)"
    );
    assert_eq!(rgba_at(&pixmap, 0, 0), (255, 255, 255, 255));
  });
}
