use fastrender::{FastRender, PreparedPaintOptions, RenderOptions, Result};

fn pixel(pixmap: &fastrender::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).unwrap();
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn video_without_poster_does_not_paint_placeholder() -> Result<()> {
  let mut renderer = FastRender::new()?;
  let html = include_str!("../../pages/fixtures/video_element_placeholder/index.html");

  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(1040, 700))?;
  let pixmap = prepared.paint_with_options(PreparedPaintOptions::new())?;

  // The <video> box sits on top of the background, but should remain transparent when there's no
  // poster/frame available.
  assert_eq!(pixel(&pixmap, 500, 200), (23, 19, 33, 255));
  Ok(())
}

#[test]
fn video_with_controls_without_poster_paints_placeholder() -> Result<()> {
  let mut renderer = FastRender::new()?;
  let html = include_str!("../../pages/fixtures/video_element_controls_placeholder/index.html");

  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(140, 100))?;
  let pixmap = prepared.paint_with_options(PreparedPaintOptions::new())?;

  // `<video controls>` typically paints an opaque UI surface even before playback. FastRender
  // doesn't implement native controls, so it should at least paint a deterministic placeholder
  // instead of leaving the element transparent. The placeholder should also avoid painting a full
  // fake control set (icons) since that tends to diverge from Chromium's auto-hidden controls.
  let surface = pixel(&pixmap, 50, 30);
  assert_ne!(surface, (0, 255, 0, 255));

  // A subtle scrubber track should be visible near the bottom bar.
  let track = pixel(&pixmap, 50, 58);
  assert!(
    track.0 >= 60 && track.1 >= 60 && track.2 >= 60 && track.3 == 255,
    "expected scrubber track pixel to be bright; got {track:?}"
  );

  // But pixels in the same bar area away from the scrubber should remain dark.
  let bar_bg = pixel(&pixmap, 50, 70);
  assert!(
    bar_bg.0 < 20 && bar_bg.1 < 20 && bar_bg.2 < 20 && bar_bg.3 == 255,
    "expected control bar background to be dark; got {bar_bg:?}"
  );
  Ok(())
}
