use fastrender::style::color::Rgba;
use fastrender::text::color_fonts::ColorFontRenderer;
use fastrender::text::font_db::{FontStretch, FontStyle, FontWeight, LoadedFont};
use fastrender::text::font_instance::FontInstance;
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use tiny_skia::GradientStop;

use super::{fail_next_allocation, failed_allocs, lock_allocator};

fn fixtures_path() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests")
    .join("fixtures")
}

fn load_sheared_font() -> LoadedFont {
  let data = std::fs::read(fixtures_path().join("fonts/colrv1-linear-shear.ttf")).unwrap();
  LoadedFont {
    id: None,
    data: Arc::new(data),
    index: 0,
    family: "ColrV1LinearShear".into(),
    weight: FontWeight::NORMAL,
    style: FontStyle::Normal,
    stretch: FontStretch::Normal,
    face_metrics_overrides: Default::default(),
    face_settings: Default::default(),
  }
}

#[test]
fn colrv1_linear_gradient_survives_gradient_stop_allocation_failure() {
  let _guard = lock_allocator();

  let renderer = ColorFontRenderer::new();

  let font_ok = load_sheared_font();
  let face_ok = font_ok.as_ttf_face().unwrap();
  let gid_ok = face_ok.glyph_index('G').unwrap().0 as u32;
  let instance_ok = FontInstance::new(&font_ok, &[]).unwrap();
  assert!(
    renderer
      .render(
        &font_ok,
        &instance_ok,
        gid_ok,
        64.0,
        0,
        &[],
        0,
        Rgba::BLACK,
        0.0,
        &[],
        None,
      )
      .is_some(),
    "expected baseline COLRv1 glyph render to succeed"
  );

  let font_fail = load_sheared_font();
  let face_fail = font_fail.as_ttf_face().unwrap();
  let gid_fail = face_fail.glyph_index('G').unwrap().0 as u32;
  let instance_fail = FontInstance::new(&font_fail, &[]).unwrap();

  // `colrv1-linear-shear.ttf` defines a 3-stop linear gradient (see fixture generator).
  let alloc_size = 3 * mem::size_of::<GradientStop>();
  let alloc_align = mem::align_of::<GradientStop>();
  let start_failures = failed_allocs();
  fail_next_allocation(alloc_size, alloc_align);

  let rendered = renderer.render(
    &font_fail,
    &instance_fail,
    gid_fail,
    64.0,
    0,
    &[],
    0,
    Rgba::BLACK,
    0.0,
    &[],
    None,
  );

  assert_eq!(
    failed_allocs(),
    start_failures + 1,
    "expected to trigger gradient stop allocation failure"
  );
  assert!(
    rendered.is_none(),
    "expected color glyph rendering to return None after allocation failure"
  );
}
