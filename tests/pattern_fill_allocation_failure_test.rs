use fastrender::geometry::{Point, Rect, Size};
use fastrender::paint::display_list::{
  DisplayItem, DisplayList, GradientSpread, GradientStop, ImageData, ImageFilterQuality,
  ImagePatternItem, ImagePatternRepeat, LinearGradientPatternItem, RadialGradientPatternItem,
};
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::text::font_loader::FontContext;
use fastrender::Rgba;
use std::alloc::{GlobalAlloc, Layout, System};
use std::mem;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
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
    if fail_size != 0
      && new_size == fail_size
      && layout.align() == FAIL_ALIGN.load(Ordering::Relaxed)
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

#[repr(C)]
#[derive(Clone, Copy)]
struct AxisSampleLayout {
  i0: u32,
  i1: u32,
  t: f32,
}

fn assert_not_white(pixmap: &tiny_skia::Pixmap) {
  let px = &pixmap.data()[..4];
  assert_ne!(px, &[255, 255, 255, 255], "expected non-white pixel, got {px:?}");
}

#[test]
fn pattern_fills_survive_allocation_failures_in_sampling_tables() {
  let _guard = LOCK.lock().unwrap();

  const WIDTH: u32 = 70_000;
  const HEIGHT: u32 = 2;

  let mut list = DisplayList::new();
  list.push(DisplayItem::LinearGradientPattern(LinearGradientPatternItem {
    dest_rect: Rect::from_xywh(0.0, 0.0, WIDTH as f32, HEIGHT as f32),
    tile_size: Size::new(1.0, 1.0),
    origin: Point::new(0.0, 0.0),
    start: Point::new(0.0, 0.0),
    end: Point::new(1.0, 0.0),
    stops: vec![
      GradientStop {
        position: 0.0,
        color: Rgba::BLACK,
      },
      GradientStop {
        position: 1.0,
        color: Rgba::BLACK,
      },
    ],
    spread: GradientSpread::Pad,
  }));

  let renderer = DisplayListRenderer::new(WIDTH, HEIGHT, Rgba::WHITE, FontContext::new()).unwrap();
  let start_failures = FAILED_ALLOCS.load(Ordering::Relaxed);
  fail_next_allocation(WIDTH as usize * mem::size_of::<u32>(), mem::align_of::<u32>());
  let pixmap = renderer.render(&list).expect("render linear gradient pattern");
  assert_eq!(
    FAILED_ALLOCS.load(Ordering::Relaxed),
    start_failures + 1,
    "expected to trigger sampling allocation failure"
  );
  assert_not_white(&pixmap);

  let mut list = DisplayList::new();
  list.push(DisplayItem::RadialGradientPattern(RadialGradientPatternItem {
    dest_rect: Rect::from_xywh(0.0, 0.0, WIDTH as f32, HEIGHT as f32),
    tile_size: Size::new(1.0, 1.0),
    origin: Point::new(0.0, 0.0),
    center: Point::new(0.5, 0.5),
    radii: Point::new(0.5, 0.5),
    stops: vec![
      GradientStop {
        position: 0.0,
        color: Rgba::BLACK,
      },
      GradientStop {
        position: 1.0,
        color: Rgba::BLACK,
      },
    ],
    spread: GradientSpread::Pad,
  }));

  let renderer = DisplayListRenderer::new(WIDTH, HEIGHT, Rgba::WHITE, FontContext::new()).unwrap();
  let start_failures = FAILED_ALLOCS.load(Ordering::Relaxed);
  fail_next_allocation(WIDTH as usize * mem::size_of::<u32>(), mem::align_of::<u32>());
  let pixmap = renderer.render(&list).expect("render radial gradient pattern");
  assert_eq!(
    FAILED_ALLOCS.load(Ordering::Relaxed),
    start_failures + 1,
    "expected to trigger sampling allocation failure"
  );
  assert_not_white(&pixmap);

  let mut list = DisplayList::new();
  let image = Arc::new(ImageData::new_pixels(1, 1, vec![255, 0, 0, 255]));
  list.push(DisplayItem::ImagePattern(ImagePatternItem {
    dest_rect: Rect::from_xywh(0.0, 0.0, WIDTH as f32, HEIGHT as f32),
    image,
    tile_size: Size::new(1.0, 1.0),
    origin: Point::new(0.0, 0.0),
    repeat: ImagePatternRepeat::Repeat,
    filter_quality: ImageFilterQuality::Linear,
  }));

  let renderer = DisplayListRenderer::new(WIDTH, HEIGHT, Rgba::WHITE, FontContext::new()).unwrap();
  let start_failures = FAILED_ALLOCS.load(Ordering::Relaxed);
  fail_next_allocation(
    WIDTH as usize * mem::size_of::<AxisSampleLayout>(),
    mem::align_of::<AxisSampleLayout>(),
  );
  let pixmap = renderer.render(&list).expect("render image pattern");
  assert_eq!(
    FAILED_ALLOCS.load(Ordering::Relaxed),
    start_failures + 1,
    "expected to trigger sampling allocation failure"
  );
  assert_not_white(&pixmap);
}

