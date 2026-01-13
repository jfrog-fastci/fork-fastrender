use crate::r#ref::{compare_images, CompareConfig};
use fastrender::{FastRender, FastRenderConfig, FontConfig, RenderOptions};

const TABLE_HTML: &str = include_str!("../wpt/tests/css/tables/border-collapse-basic-001.html");
const REFERENCE_HTML: &str =
  include_str!("../wpt/tests/css/tables/border-collapse-basic-001-ref.html");

#[test]
fn border_collapse_basic_001_matches_reference_at_viewport_origin() {
  // This is a paint-level regression test mirroring WPT `border-collapse-basic-001`.
  //
  // The table starts at the viewport origin and uses `border-collapse: collapse` with 10px
  // borders. Historically we've regressed collapsed border geometry when border edges touch the
  // viewport origin; rendering both the test and its reference HTML and comparing pixels catches
  // those issues without needing to store a golden PNG.
  let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
  let mut renderer = FastRender::with_config(config).expect("Failed to create renderer");

  // Keep the render small and deterministic (matches our WPT harness settings for this test).
  let options = RenderOptions::new()
    .with_viewport(60, 60)
    .with_device_pixel_ratio(1.0);

  let table = renderer
    .render_html_with_options(TABLE_HTML, options.clone())
    .expect("Failed to render table HTML");
  let reference = renderer
    .render_html_with_options(REFERENCE_HTML, options)
    .expect("Failed to render reference HTML");

  let diff = compare_images(&table, &reference, &CompareConfig::strict());
  assert!(
    diff.is_match(),
    "border-collapse-basic-001 mismatch: {}",
    diff.summary()
  );
}

