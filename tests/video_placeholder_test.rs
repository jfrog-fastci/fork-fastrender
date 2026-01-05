use fastrender::{FastRender, PreparedPaintOptions, RenderOptions, Result};

fn pixel(pixmap: &fastrender::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).unwrap();
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn video_without_poster_does_not_paint_placeholder() -> Result<()> {
  let mut renderer = FastRender::new()?;
  let html = include_str!("pages/fixtures/video_element_placeholder/index.html");

  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(1040, 700))?;
  let pixmap = prepared.paint_with_options(PreparedPaintOptions::new())?;

  // The <video> box sits on top of the background, but should remain transparent when there's no
  // poster/frame available.
  assert_eq!(pixel(&pixmap, 500, 200), (23, 19, 33, 255));
  Ok(())
}
