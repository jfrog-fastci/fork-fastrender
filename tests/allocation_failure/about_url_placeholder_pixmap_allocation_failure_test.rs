use fastrender::error::{Error, RenderError};
use fastrender::image_loader::ImageCache;
use fastrender::style::types::OrientationTransform;
use super::{fail_next_allocation, failed_allocs, lock_allocator};

#[test]
fn about_url_placeholder_pixmap_survives_allocation_failure() {
  let _guard = lock_allocator();

  let cache = ImageCache::new();
  let start_failures = failed_allocs();

  // Fail the `Vec<u8>` allocation for the 1×1 pixmap buffer (4 RGBA bytes).
  fail_next_allocation(4, 1);

  let result = cache.load_raster_pixmap("about:blank", OrientationTransform::IDENTITY, false);

  assert_eq!(
    failed_allocs(),
    start_failures + 1,
    "expected to trigger placeholder pixmap allocation failure"
  );

  match result {
    Err(Error::Render(RenderError::InvalidParameters { message })) => {
      assert!(
        message.contains("pixmap allocation failed"),
        "unexpected RenderError message: {message}"
      );
    }
    other => panic!("expected RenderError after allocation failure, got {other:?}"),
  }
}
