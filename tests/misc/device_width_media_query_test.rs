use fastrender::api::FastRender;

#[test]
fn device_width_media_queries_use_headless_chrome_screen_size_by_default() {
  let mut renderer = FastRender::new().unwrap();
  let html = r#"
    <!doctype html>
    <style>
      html, body { margin: 0; }
      #box { width: 100vw; height: 100vh; background: rgb(255, 0, 0); }
      @media (max-device-width: 900px) {
        /* Chrome headless reports a default screen width of 800px, so this matches. */
        #box { background: rgb(0, 200, 0); }
      }
      @media (max-width: 900px) {
        /* The viewport is wider than 900px, so this should not match. */
        #box { background: rgb(0, 0, 255); }
      }
    </style>
    <div id="box"></div>
  "#;

  let pixmap = renderer.render_html(html, 1200, 800).unwrap();
  let pixel = pixmap.pixel(10, 10).unwrap();
  assert_eq!(
    (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
    (0, 200, 0, 255),
    "expected max-device-width media query to apply (green background)"
  );
}
