#![cfg(feature = "avif")]

use fastrender::image_loader::ImageCache;
use std::path::PathBuf;
use url::Url;

fn fixture_file_url(rel_path: &str) -> String {
  let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel_path);
  Url::from_file_path(path)
    .unwrap_or_else(|_| panic!("failed to build file URL for {rel_path}"))
    .to_string()
}

#[test]
fn image_cache_decodes_avif_pixels() {
  let cache = ImageCache::new();

  // How-To Geek fixtures are AVIF-heavy; if AVIF decodes fail we end up with blank (white) pages.
  let url = fixture_file_url(
    "tests/pages/fixtures/howtogeek.com/assets/fb5e72f6b237f006dd9df83e1aa8e008.avif",
  );
  let img = cache.load(&url).expect("load avif fixture");

  let rgba = img.image.to_rgba8();
  let bytes = rgba.as_raw();
  assert!(
    !bytes.is_empty(),
    "decoded AVIF should contain RGBA pixels (got empty buffer)"
  );

  let mut alpha_max = 0u8;
  let mut has_non_white = false;
  for px in bytes.chunks_exact(4) {
    alpha_max = alpha_max.max(px[3]);
    has_non_white |= px[0] != 0xFF || px[1] != 0xFF || px[2] != 0xFF;
    if alpha_max == 0xFF && has_non_white {
      break;
    }
  }

  assert!(
    alpha_max > 0,
    "decoded AVIF unexpectedly has alpha=0 for all pixels (fully transparent)"
  );
  assert!(
    has_non_white,
    "decoded AVIF unexpectedly contains only white pixels"
  );
}
