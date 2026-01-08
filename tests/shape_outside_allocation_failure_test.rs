use fastrender::geometry::{Rect, Size};
use fastrender::image_loader::ImageCache;
use fastrender::layout::float_shape::build_float_shape;
use fastrender::style::types::{ReferenceBox, ShapeOutside};
use fastrender::style::ComputedStyle;
use fastrender::text::font_loader::FontContext;
use std::alloc::{GlobalAlloc, Layout, System};
use std::mem;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

struct FailingAllocator;

static FAIL_SIZE: AtomicUsize = AtomicUsize::new(0);
static FAIL_ALIGN: AtomicUsize = AtomicUsize::new(0);
static FAILED_ALLOCS: AtomicUsize = AtomicUsize::new(0);

fn fail_next_allocation(size: usize, align: usize) {
  FAIL_ALIGN.store(align, Ordering::Relaxed);
  FAIL_SIZE.store(size, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for FailingAllocator {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    let fail_size = FAIL_SIZE.load(Ordering::Relaxed);
    if fail_size != 0
      && layout.size() == fail_size
      && layout.align() == FAIL_ALIGN.load(Ordering::Relaxed)
    {
      FAIL_SIZE.store(0, Ordering::Relaxed);
      FAILED_ALLOCS.fetch_add(1, Ordering::Relaxed);
      return std::ptr::null_mut();
    }
    System.alloc(layout)
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    let fail_size = FAIL_SIZE.load(Ordering::Relaxed);
    if fail_size != 0
      && layout.size() == fail_size
      && layout.align() == FAIL_ALIGN.load(Ordering::Relaxed)
    {
      FAIL_SIZE.store(0, Ordering::Relaxed);
      FAILED_ALLOCS.fetch_add(1, Ordering::Relaxed);
      return std::ptr::null_mut();
    }
    System.alloc_zeroed(layout)
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    let fail_size = FAIL_SIZE.load(Ordering::Relaxed);
    if fail_size != 0 && new_size == fail_size && layout.align() == FAIL_ALIGN.load(Ordering::Relaxed)
    {
      FAIL_SIZE.store(0, Ordering::Relaxed);
      FAILED_ALLOCS.fetch_add(1, Ordering::Relaxed);
      return std::ptr::null_mut();
    }
    System.realloc(ptr, layout, new_size)
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    System.dealloc(ptr, layout)
  }
}

#[global_allocator]
static GLOBAL: FailingAllocator = FailingAllocator;

static LOCK: Mutex<()> = Mutex::new(());

#[test]
fn shape_outside_span_buffers_survive_allocation_failure() {
  let _guard = LOCK.lock().unwrap();

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

  let start_failures = FAILED_ALLOCS.load(Ordering::Relaxed);
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
    FAILED_ALLOCS.load(Ordering::Relaxed),
    start_failures + 1,
    "expected to trigger span buffer allocation failure"
  );
  assert_eq!(shape, None, "expected shape-outside to fall back on allocation failure");
}

