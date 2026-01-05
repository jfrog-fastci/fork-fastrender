use fastrender::{FastRender, PreparedPaintOptions, RenderOptions, Result, Rgba};
use std::fs;
use std::path::PathBuf;

fn pixel(pixmap: &fastrender::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).unwrap();
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn scroll_driven_custom_property_updates_var_dependent_border_width() -> Result<()> {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let fixture_path =
    repo_root.join("tests/pages/fixtures/scroll_driven_custom_property/index.html");
  let html = fs::read_to_string(&fixture_path)
    .unwrap_or_else(|e| panic!("read fixture {}: {e}", fixture_path.display()));

  let mut renderer = FastRender::new()?;
  let prepared = renderer.prepare_html(&html, RenderOptions::new().with_viewport(110, 50))?;
  let pixmap = prepared.paint_with_options(
    PreparedPaintOptions::new()
      .with_scroll(0.0, 0.0)
      .with_background(Rgba::new(0, 0, 0, 1.0)),
  )?;

  // Scroll range > 0 activates the scroll(self) timeline, which sets --can-scroll to 1 and therefore
  // makes the border visible.
  assert_eq!(pixel(&pixmap, 25, 49), (255, 0, 0, 255));

  // Scroll range == 0 leaves the timeline inactive, keeping --can-scroll at 0 and therefore hiding
  // the border.
  assert_eq!(pixel(&pixmap, 85, 49), (0, 255, 0, 255));
  Ok(())
}

#[test]
fn scroll_driven_custom_property_does_not_clobber_late_border_overrides() -> Result<()> {
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let fixture_path = repo_root.join(
    "tests/pages/fixtures/scroll_driven_custom_property_shorthand_override/index.html",
  );
  let html = fs::read_to_string(&fixture_path)
    .unwrap_or_else(|e| panic!("read fixture {}: {e}", fixture_path.display()));

  let mut renderer = FastRender::new()?;
  let prepared = renderer.prepare_html(&html, RenderOptions::new().with_viewport(110, 50))?;
  let pixmap = prepared.paint_with_options(
    PreparedPaintOptions::new()
      .with_scroll(0.0, 0.0)
      .with_background(Rgba::new(0, 0, 0, 1.0)),
  )?;

  // The scrollable box activates the scroll(self) timeline, which sets --can-scroll to 1. The
  // border-bottom shorthand depends on --can-scroll, but the border-bottom-color longhand override
  // is var-free and must win.
  assert_eq!(pixel(&pixmap, 25, 49), (0, 0, 255, 255));

  // The non-scrollable box keeps --can-scroll at 0, so the border stays hidden and the bottom pixel
  // remains the green background.
  assert_eq!(pixel(&pixmap, 85, 49), (0, 255, 0, 255));
  Ok(())
}
