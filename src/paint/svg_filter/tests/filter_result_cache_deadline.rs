use super::super::{
  apply_svg_filter, svg_filter_test_guard, ColorInterpolationFilters, FilterPrimitive, FilterStep,
  FilterCacheConfig, SvgFilter, SvgFilterRegion, SvgFilterUnits, SvgLength,
};
use crate::error::{RenderError, RenderStage};
use crate::geometry::Rect;
use crate::render_control::{with_deadline, RenderDeadline};
use crate::Rgba;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tiny_skia::{Pixmap, PremultipliedColorU8};

#[test]
fn svg_filter_result_cache_key_respects_deadline() {
  let _guard = svg_filter_test_guard();

  // Ensure filter result caching is enabled and isolated for this test. We need a cache *hit* on the
  // second `apply_svg_filter` call so that:
  // - without deadline-aware hashing, the function would return `Ok(())` without ever checking the
  //   deadline (regression reproduction);
  // - with deadline-aware hashing, the cache key fingerprint scan can be interrupted.
  let previous_config = super::super::filter_result_cache_config();
  struct CacheConfigGuard(FilterCacheConfig);
  impl Drop for CacheConfigGuard {
    fn drop(&mut self) {
      super::super::reset_filter_result_cache_for_tests(self.0);
    }
  }
  let _cache_guard = CacheConfigGuard(previous_config);
  super::super::reset_filter_result_cache_for_tests(FilterCacheConfig {
    max_items: 8,
    max_bytes: 16 * 1024 * 1024,
  });
  assert_eq!(super::super::filter_result_cache_len(), 0);

  let mut source = Pixmap::new(512, 512).expect("pixmap");
  source
    .pixels_mut()
    .fill(PremultipliedColorU8::from_rgba(255, 0, 0, 255).unwrap());

  let bbox = Rect::from_xywh(0.0, 0.0, 512.0, 512.0);
  let mut filter = SvgFilter {
    color_interpolation_filters: ColorInterpolationFilters::SRGB,
    steps: vec![FilterStep {
      result: None,
      color_interpolation_filters: None,
      primitive: FilterPrimitive::Flood {
        color: Rgba::from_rgba8(0, 255, 0, 255),
        opacity: 1.0,
      },
      region: None,
    }],
    region: SvgFilterRegion {
      x: SvgLength::Number(0.0),
      y: SvgLength::Number(0.0),
      width: SvgLength::Number(512.0),
      height: SvgLength::Number(512.0),
      units: SvgFilterUnits::UserSpaceOnUse,
    },
    filter_res: None,
    primitive_units: SvgFilterUnits::UserSpaceOnUse,
    fingerprint: 0,
  };
  filter.refresh_fingerprint();

  // Seed the cache with a successful render.
  let mut first = source.clone();
  apply_svg_filter(&filter, &mut first, 1.0, bbox).expect("seed apply");
  assert!(
    super::super::filter_result_cache_len() > 0,
    "expected svg filter result cache to be populated"
  );

  // Install a deadline that cancels after the first `check_active`, then invoke the filter.
  // With a warmed cache, the second call should exit early after the cache hit, so the only place
  // that can observe the cancellation is cache key fingerprinting.
  let calls = Arc::new(AtomicUsize::new(0));
  let calls_cb = Arc::clone(&calls);
  let cancel = Arc::new(move || calls_cb.fetch_add(1, Ordering::SeqCst) >= 1);
  let deadline = RenderDeadline::new(None, Some(cancel));

  let mut second = source.clone();
  let result = with_deadline(Some(&deadline), || apply_svg_filter(&filter, &mut second, 1.0, bbox));

  assert!(
    matches!(
      result,
      Err(RenderError::Timeout {
        stage: RenderStage::Paint,
        ..
      })
    ),
    "expected timeout"
  );
  assert!(
    calls.load(Ordering::SeqCst) >= 2,
    "expected cancel callback to be queried more than once, got {}",
    calls.load(Ordering::SeqCst)
  );
}
