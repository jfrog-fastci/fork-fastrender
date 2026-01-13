use fastrender::api::BrowserDocument;
use fastrender::{PreparedPaintOptions, RenderOptions, Result};
use image::Rgba as ImageRgba;
use image::RgbaImage;
use tempfile::tempdir;
use url::Url;

#[test]
fn scroll_blit_disabled_when_image_cache_epoch_changes() -> Result<()> {
  // This test models the stale-pixels failure mode for a scroll-blit fast path:
  //
  // 1) Render a frame where an <img> is missing (resource not yet present).
  // 2) Populate the ImageCache with the now-available image *without* invalidating layout.
  // 3) Trigger a small scroll repaint that would normally be eligible for scroll-blit.
  // 4) Assert the scroll repaint matches a fresh full repaint.
  //
  // Without an epoch gate, a scroll-blit implementation that only repaints the exposed stripes
  // would reuse the old pixels in the image region, producing a mismatch.

  let dir = tempdir().expect("temp dir");
  let img_path = dir.path().join("late.png");
  let img_url = Url::from_file_path(&img_path)
    .expect("file url")
    .to_string();

  let html = format!(
    r#"
      <style>
        body {{ margin: 0; background: rgb(255, 255, 255); }}
        .spacer {{ height: 40px; }}
        img {{ display: block; width: 40px; height: 40px; }}
        .tail {{ height: 300px; }}
      </style>
      <div class="spacer"></div>
      <img src="{img_url}">
      <div class="tail"></div>
    "#
  );

  let mut doc =
    BrowserDocument::from_html(&html, RenderOptions::new().with_viewport(120, 120))?;

  // First frame: the image resource does not exist yet.
  let _frame1 = doc.render_frame_with_scroll_state()?;
  let epoch0 = doc.image_cache().epoch();

  // Make the image appear and explicitly decode it into the ImageCache without touching layout.
  let png = RgbaImage::from_pixel(8, 8, ImageRgba([0, 0, 255, 255]));
  png.save(&img_path).expect("write png");
  doc.image_cache().load(&img_url)?;

  let epoch1 = doc.image_cache().epoch();
  assert_ne!(
    epoch0, epoch1,
    "expected ImageCache epoch to change after inserting a newly decoded image"
  );

  // Trigger a small scroll delta; scroll-blit implementations typically only repaint exposed
  // stripes for this case.
  doc.set_scroll(0.0, 5.0);
  let scrolled = doc.paint_from_cache_frame_with_deadline(None)?;

  // Baseline: force a full repaint from the cached PreparedDocument (bypasses any scroll-blit path
  // that might live in BrowserDocument/UI layers).
  let prepared = doc.prepared().expect("prepared document");
  let expected = prepared.paint_with_options_frame(PreparedPaintOptions {
    scroll: Some(scrolled.scroll_state.clone()),
    viewport: None,
    background: None,
    animation_time: None,
    ..PreparedPaintOptions::default()
  })?;

  assert_eq!(
    scrolled.pixmap.data(),
    expected.pixmap.data(),
    "scroll repaint did not match a full repaint; scroll-blit may have reused stale pixels"
  );

  Ok(())
}
