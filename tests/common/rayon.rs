use std::sync::Once;

use fastrender::{FastRender, FastRenderConfig, FontConfig};

pub fn init_rayon_for_tests(_num_threads: usize) {
  static INIT: Once = Once::new();

  INIT.call_once(|| {
    // Constructing a renderer triggers FastRender's safe global Rayon initialisation (including
    // CI-friendly fallback logic). Integration tests should not configure the Rayon global pool
    // directly, since it is process-global and order-dependent.
    let config = FastRenderConfig::new().with_font_sources(FontConfig::bundled_only());
    let _ = FastRender::with_config(config).expect("init renderer");
  });
}

