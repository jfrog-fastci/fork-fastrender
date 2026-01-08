use fastrender::api::{FastRender, RenderOptions};
use fastrender::style::media::MediaType;

#[test]
fn fit_canvas_to_content_renders_all_pages() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
        </style>
      </head>
      <body>
        <div style="height: 220px; background: red;"></div>
        <div style="height: 220px; background: blue;"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::builder()
    .viewport_size(200, 200)
    .fit_canvas_to_content(true)
    .build()
    .unwrap();

  let pixmap = renderer
    .render_html_with_options(
      html,
      RenderOptions::new()
        .with_viewport(200, 200)
        .with_media_type(MediaType::Print),
    )
    .unwrap();

  assert!(
    pixmap.height() >= 400,
    "expected pixmap height to include multiple pages, got {}",
    pixmap.height()
  );
}
