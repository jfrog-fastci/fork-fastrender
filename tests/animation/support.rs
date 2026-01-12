use std::sync::Once;

static INIT: Once = Once::new();

pub fn ensure_test_env() {
  INIT.call_once(|| {
    // FastRender uses Rayon for parallel layout/paint. Rayon defaults to the host CPU count, which
    // can exceed sandbox thread budgets and cause the global pool init to fail.
    crate::common::rayon_test_util::init_rayon_for_tests(1);
  });
}

pub fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).unwrap_or_else(|| {
    panic!(
      "pixel({x}, {y}) out of bounds (pixmap size {}x{})",
      pixmap.width(),
      pixmap.height()
    )
  });
  (px.red(), px.green(), px.blue(), px.alpha())
}
