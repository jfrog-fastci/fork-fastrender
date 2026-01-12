use fastrender::api::FastRender;
use fastrender::RenderOptions;

#[test]
fn css_content_url_replaces_img_source() {
  let _lock = super::global_test_lock();
  let mut renderer = FastRender::new().unwrap();

  // Chrome applies `content: url(...)` to replaced elements like `<img>`, allowing CSS to override
  // the image source. Real-world pages (e.g. discord.com language flags) rely on this.
  let red_svg_b64 = "PHN2ZyB4bWxucz0naHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmcnIHdpZHRoPScxJyBoZWlnaHQ9JzEnPjxyZWN0IHdpZHRoPScxJyBoZWlnaHQ9JzEnIGZpbGw9J3JlZCcvPjwvc3ZnPg==";
  let red_data_url = format!("data:image/svg+xml;base64,{red_svg_b64}");

  let html = format!(
    r#"
    <html>
      <head>
        <style>
          html, body {{ margin: 0; padding: 0; background: #fff; }}
          img#flag {{
            display: block;
            width: 20px;
            height: 20px;
            content: url("{red_data_url}");
          }}
        </style>
      </head>
      <body>
        <img id="flag">
      </body>
    </html>
    "#
  );

  let pixmap = renderer
    .render_html_with_options(&html, RenderOptions::new().with_viewport(40, 40))
    .expect("render");

  let pixel = pixmap.pixel(10, 10).expect("pixel in bounds");
  assert_eq!(
    (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
    (255, 0, 0, 255),
    "expected `content: url(...)` to paint the image for <img>"
  );
}
