use fastrender::{FastRender, FastRenderConfig, RenderOptions};

#[test]
fn meta_viewport_zoom_preserves_requested_output_size() {
  let config = FastRenderConfig::new()
    .with_default_viewport(1040, 1240)
    .with_meta_viewport(true);
  let mut renderer = FastRender::with_config(config).expect("renderer should construct");

  // Meta viewport "width=1280" causes a zoom < 1 for the 1040px requested viewport, which in turn
  // produces a fractional visual viewport height. The renderer should still output the requested
  // device pixel size (1040x1240 at DPR=1), matching browser screenshots.
  let html = r#"
    <!doctype html>
    <html>
      <head>
        <meta name="viewport" content="width=1280">
        <style>
          body { margin: 0; }
        </style>
      </head>
      <body></body>
    </html>
  "#;

  let options = RenderOptions::new()
    .with_viewport(1040, 1240)
    .with_device_pixel_ratio(1.0);

  let pixmap = renderer
    .render_html_with_options(html, options.clone())
    .expect("render should succeed");
  assert_eq!((pixmap.width(), pixmap.height()), (1040, 1240));

  let prepared = renderer
    .prepare_html(html, options)
    .expect("prepare should succeed");
  let prepared_pixmap = prepared.paint_default().expect("paint should succeed");
  assert_eq!(
    (prepared_pixmap.width(), prepared_pixmap.height()),
    (1040, 1240)
  );
}

