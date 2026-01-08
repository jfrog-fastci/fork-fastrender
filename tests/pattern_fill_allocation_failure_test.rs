use fastrender::geometry::{Point, Rect, Size};
use fastrender::paint::display_list::{
  BlendMode, BorderRadii, DisplayItem, DisplayList, FillRectItem, GradientSpread, GradientStop,
  ImageData, ImageFilterQuality, ImagePatternItem, ImagePatternRepeat, LinearGradientPatternItem,
  MaskReferenceRects, ResolvedMask, ResolvedMaskImage, ResolvedMaskLayer, StackingContextItem,
  RadialGradientPatternItem,
};
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::text::font_loader::FontContext;
use fastrender::Rgba;
use fastrender::style::types::{
  BackfaceVisibility, BackgroundPosition, BackgroundPositionComponent, BackgroundRepeat,
  BackgroundSize, BackgroundSizeComponent, MaskClip, MaskComposite, MaskMode, MaskOrigin,
  TransformStyle,
};
use fastrender::style::values::Length;
use std::alloc::{GlobalAlloc, Layout, System};
use std::mem;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

struct FailingAllocator;

static FAIL_SIZE: AtomicUsize = AtomicUsize::new(0);
static FAIL_ALIGN: AtomicUsize = AtomicUsize::new(0);
static FAIL_SKIP_MATCHES: AtomicUsize = AtomicUsize::new(0);
static FAILED_ALLOCS: AtomicUsize = AtomicUsize::new(0);

fn fail_next_allocation(size: usize, align: usize) {
  FAIL_ALIGN.store(align, Ordering::Relaxed);
  FAIL_SIZE.store(size, Ordering::Relaxed);
  FAIL_SKIP_MATCHES.store(0, Ordering::Relaxed);
}

fn fail_nth_allocation(size: usize, align: usize, skip_matches: usize) {
  FAIL_ALIGN.store(align, Ordering::Relaxed);
  FAIL_SIZE.store(size, Ordering::Relaxed);
  FAIL_SKIP_MATCHES.store(skip_matches, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for FailingAllocator {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    let fail_size = FAIL_SIZE.load(Ordering::Relaxed);
    if fail_size != 0
      && layout.size() == fail_size
      && layout.align() == FAIL_ALIGN.load(Ordering::Relaxed)
    {
      let skip = FAIL_SKIP_MATCHES.load(Ordering::Relaxed);
      if skip > 0 {
        FAIL_SKIP_MATCHES.store(skip - 1, Ordering::Relaxed);
        return System.alloc(layout);
      }
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
      let skip = FAIL_SKIP_MATCHES.load(Ordering::Relaxed);
      if skip > 0 {
        FAIL_SKIP_MATCHES.store(skip - 1, Ordering::Relaxed);
        return System.alloc_zeroed(layout);
      }
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
      let skip = FAIL_SKIP_MATCHES.load(Ordering::Relaxed);
      if skip > 0 {
        FAIL_SKIP_MATCHES.store(skip - 1, Ordering::Relaxed);
        return System.realloc(ptr, layout, new_size);
      }
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

fn simple_mask(image: ResolvedMaskImage, mode: MaskMode, bounds: Rect) -> ResolvedMask {
  let default_position = BackgroundPosition::Position {
    x: BackgroundPositionComponent {
      alignment: 0.0,
      offset: Length::percent(0.0),
    },
    y: BackgroundPositionComponent {
      alignment: 0.0,
      offset: Length::percent(0.0),
    },
  };
  ResolvedMask {
    layers: vec![ResolvedMaskLayer {
      image,
      repeat: BackgroundRepeat::no_repeat(),
      position: default_position,
      size: BackgroundSize::Explicit(BackgroundSizeComponent::Auto, BackgroundSizeComponent::Auto),
      origin: MaskOrigin::BorderBox,
      clip: MaskClip::BorderBox,
      mode,
      composite: MaskComposite::Add,
    }],
    color: Rgba::BLACK,
    font_size: 16.0,
    root_font_size: 16.0,
    viewport: None,
    rects: MaskReferenceRects {
      border: bounds,
      padding: bounds,
      content: bounds,
    },
  }
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

  let mask_bounds = Rect::from_xywh(0.0, 0.0, 1.0, 1.0);
  let mask_w = 10_001u32;
  let mask_h = 1u32;
  let mut mask_pixels = vec![0u8; (mask_w * mask_h * 4) as usize];
  for px in mask_pixels.chunks_exact_mut(4) {
    px[3] = 255;
  }
  let mask_image = ImageData::new(mask_w, mask_h, 1.0, 1.0, mask_pixels);
  let mask = simple_mask(ResolvedMaskImage::Raster(mask_image), MaskMode::Luminance, mask_bounds);

  let mut list = DisplayList::new();
  list.push(DisplayItem::PushStackingContext(StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: true,
    has_backdrop_sensitive_descendants: false,
    bounds: mask_bounds,
    plane_rect: mask_bounds,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: true,
    transform: None,
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: Some(mask),
    has_clip_path: false,
  }));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: mask_bounds,
    color: Rgba::RED,
  }));
  list.push(DisplayItem::PopStackingContext);

  let renderer = DisplayListRenderer::new(1, 1, Rgba::WHITE, FontContext::new()).unwrap();
  let start_failures = FAILED_ALLOCS.load(Ordering::Relaxed);
  let mask_bytes = (mask_w as usize) * (mask_h as usize) * 4;
  fail_nth_allocation(mask_bytes, mem::align_of::<u8>(), 1);
  let pixmap = renderer.render(&list).expect("render luminance mask with failed alloc");
  assert_eq!(
    FAILED_ALLOCS.load(Ordering::Relaxed),
    start_failures + 1,
    "expected to trigger mask conversion allocation failure"
  );
  assert_eq!(
    &pixmap.data()[..4],
    &[255, 0, 0, 255],
    "expected mask layer to be skipped after allocation failure"
  );
}
