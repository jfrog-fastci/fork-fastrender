use fastrender::geometry::{Rect, Size};
use fastrender::image_loader::ImageCache;
use fastrender::layout::float_shape::build_float_shape;
use fastrender::style::types::{ReferenceBox, ShapeOutside};
use fastrender::style::ComputedStyle;
use fastrender::text::font_loader::FontContext;
use std::mem;

use super::{fail_next_allocation, failed_allocs, lock_allocator};

#[test]
fn shape_outside_span_buffers_survive_allocation_failure() {
  let _guard = lock_allocator();

  let mut style = ComputedStyle::default();
  style.shape_outside = ShapeOutside::Box(ReferenceBox::MarginBox);

  let height_px = 1_000_000.0;
  let margin_box = Rect::from_xywh(0.0, 0.0, 10.0, height_px);
  let border_box = margin_box;
  let containing_block = Size::new(10.0, height_px);
  let viewport = Size::new(100.0, 100.0);
  let font_ctx = FontContext::new();
  let image_cache = ImageCache::new();

  let span_len = height_px.ceil() as usize;
  let alloc_size = span_len * mem::size_of::<Option<(f32, f32)>>();
  let alloc_align = mem::align_of::<Option<(f32, f32)>>();

  let start_failures = failed_allocs();
  fail_next_allocation(alloc_size, alloc_align);
  let shape = build_float_shape(
    &style,
    margin_box,
    border_box,
    containing_block,
    viewport,
    &font_ctx,
    &image_cache,
  )
  .expect("build float shape should not error");

  assert_eq!(
    failed_allocs(),
    start_failures + 1,
    "expected to trigger span buffer allocation failure"
  );
  assert_eq!(shape, None, "expected shape-outside to fall back on allocation failure");
}
