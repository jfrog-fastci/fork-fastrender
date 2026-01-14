use fastrender::ui::chrome_frame::geometry::{
  element_border_rect_by_id, element_border_rect_by_id_with_viewport_scroll,
};
use fastrender::{FastRender, FontConfig, Point, Rect, RenderOptions, Result};

fn renderer_for_tests() -> FastRender {
  FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer")
}

fn assert_rect_close(actual: Rect, expected: Rect, tolerance: f32) {
  assert!(
    (actual.x() - expected.x()).abs() <= tolerance,
    "expected x={}±{}, got {}",
    expected.x(),
    tolerance,
    actual.x()
  );
  assert!(
    (actual.y() - expected.y()).abs() <= tolerance,
    "expected y={}±{}, got {}",
    expected.y(),
    tolerance,
    actual.y()
  );
  assert!(
    (actual.width() - expected.width()).abs() <= tolerance,
    "expected width={}±{}, got {}",
    expected.width(),
    tolerance,
    actual.width()
  );
  assert!(
    (actual.height() - expected.height()).abs() <= tolerance,
    "expected height={}±{}, got {}",
    expected.height(),
    tolerance,
    actual.height()
  );
}

#[test]
fn element_border_rect_by_id_finds_content_frame() -> Result<()> {
  let html = r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #header { height: 40px; }
      #content-frame { height: 100px; }
    </style>
  </head>
  <body>
    <div id="header"></div>
    <div id="content-frame"></div>
  </body>
</html>"#;
  let mut renderer = renderer_for_tests();
  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(200, 200))?;
  let rect = element_border_rect_by_id(&prepared, "content-frame").expect("content-frame rect");
  assert_rect_close(rect, Rect::from_xywh(0.0, 40.0, 200.0, 100.0), 0.5);
  Ok(())
}

#[test]
fn element_border_rect_by_id_returns_none_for_missing_element() -> Result<()> {
  let html = r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #header { height: 40px; }
    </style>
  </head>
  <body>
    <div id="header"></div>
  </body>
</html>"#;
  let mut renderer = renderer_for_tests();
  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(200, 200))?;
  assert!(element_border_rect_by_id(&prepared, "content-frame").is_none());
  Ok(())
}

#[test]
fn element_border_rect_by_id_subtracts_viewport_scroll() -> Result<()> {
  let html = r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #header { height: 40px; }
      #content-frame { height: 100px; }
    </style>
  </head>
  <body>
    <div id="header"></div>
    <div id="content-frame"></div>
  </body>
</html>"#;
  let mut renderer = renderer_for_tests();
  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(200, 200))?;
  let rect = element_border_rect_by_id_with_viewport_scroll(
    &prepared,
    "content-frame",
    Point::new(0.0, 10.0),
  )
  .expect("content-frame rect");
  assert_rect_close(rect, Rect::from_xywh(0.0, 30.0, 200.0, 100.0), 0.5);
  Ok(())
}
