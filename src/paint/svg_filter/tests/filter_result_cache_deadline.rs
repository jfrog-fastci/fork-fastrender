use super::super::{
  apply_svg_filter, svg_filter_test_guard, ColorInterpolationFilters, FilterPrimitive, FilterStep,
  SvgFilter, SvgFilterRegion, SvgFilterUnits, SvgLength,
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

  // Install a deadline that cancels after the first `check_active`, then invoke the filter.
  // Without deadline-aware cache key hashing this would succeed: the filter itself is cheap and
  // would not hit a second deadline check.
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
