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
fn image_cache_preserves_gif_alpha_metadata() {
  let cache = ImageCache::new();

  // This GIF includes a Graphics Control Extension with the transparency flag set.
  let gif_with_alpha =
    fixture_file_url("tests/pages/fixtures/ft.com/assets/ef1955ae757c8b966c83248350331bd3.gif");
  let img = cache.load(&gif_with_alpha).expect("load gif with alpha");
  assert!(
    img.has_alpha,
    "expected gif transparency metadata to be preserved (has_alpha=true)"
  );

  // This GIF omits transparency (GCE packed field bit 0 is unset), so it should be treated as a
  // luminance mask under `mask-mode: match-source`.
  let gif_without_alpha = fixture_file_url(
    "tests/pages/fixtures/slashdot.org/assets/4e0705327480ad2323cb03d9c450ffca.gif",
  );
  let img = cache
    .load(&gif_without_alpha)
    .expect("load gif without alpha");
  assert!(
    !img.has_alpha,
    "expected gif without transparency to report has_alpha=false"
  );
}

#[test]
fn image_cache_preserves_webp_alpha_metadata() {
  let cache = ImageCache::new();

  // Lossless WebP fixture with the alpha flag set in the VP8L header.
  let webp_with_alpha = fixture_file_url("tests/fixtures/avif/solid.webp");
  let img = cache.load(&webp_with_alpha).expect("load webp with alpha");
  assert!(
    img.has_alpha,
    "expected webp alpha metadata to be preserved (has_alpha=true)"
  );

  // Lossless WebP without alpha (VP8L header alpha bit unset).
  let webp_without_alpha = fixture_file_url(
    "tests/pages/fixtures/foxnews.com/assets/25aa2ed5a0afc0c31b18c51de6df7437.webp",
  );
  let img = cache
    .load(&webp_without_alpha)
    .expect("load webp without alpha");
  assert!(
    !img.has_alpha,
    "expected webp without alpha to report has_alpha=false"
  );
}
