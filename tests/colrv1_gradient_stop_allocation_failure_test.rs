use fastrender::style::color::Rgba;
use fastrender::text::color_fonts::ColorFontRenderer;
use fastrender::text::font_db::{FontStretch, FontStyle, FontWeight, LoadedFont};
use fastrender::text::font_instance::FontInstance;
use std::alloc::{GlobalAlloc, Layout, System};
use std::mem;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tiny_skia::GradientStop;

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

fn fixtures_path() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests")
    .join("fixtures")
}

fn load_sheared_font() -> LoadedFont {
  let data = std::fs::read(fixtures_path().join("fonts/colrv1-linear-shear.ttf")).unwrap();
  LoadedFont {
    id: None,
    data: Arc::new(data),
    index: 0,
    family: "ColrV1LinearShear".into(),
    weight: FontWeight::NORMAL,
    style: FontStyle::Normal,
    stretch: FontStretch::Normal,
    face_metrics_overrides: Default::default(),
    face_settings: Default::default(),
  }
}

#[test]
fn colrv1_linear_gradient_survives_gradient_stop_allocation_failure() {
  let _guard = LOCK.lock().unwrap();

  let renderer = ColorFontRenderer::new();

  let font_ok = load_sheared_font();
  let face_ok = font_ok.as_ttf_face().unwrap();
  let gid_ok = face_ok.glyph_index('G').unwrap().0 as u32;
  let instance_ok = FontInstance::new(&font_ok, &[]).unwrap();
  assert!(
    renderer
      .render(
        &font_ok,
        &instance_ok,
        gid_ok,
        64.0,
        0,
        &[],
        0,
        Rgba::BLACK,
        0.0,
        &[],
        None,
      )
      .is_some(),
    "expected baseline COLRv1 glyph render to succeed"
  );

  let font_fail = load_sheared_font();
  let face_fail = font_fail.as_ttf_face().unwrap();
  let gid_fail = face_fail.glyph_index('G').unwrap().0 as u32;
  let instance_fail = FontInstance::new(&font_fail, &[]).unwrap();

  // `colrv1-linear-shear.ttf` defines a 3-stop linear gradient (see fixture generator).
  let alloc_size = 3 * mem::size_of::<GradientStop>();
  let alloc_align = mem::align_of::<GradientStop>();
  let start_failures = FAILED_ALLOCS.load(Ordering::Relaxed);
  fail_next_allocation(alloc_size, alloc_align);

  let rendered = renderer.render(
    &font_fail,
    &instance_fail,
    gid_fail,
    64.0,
    0,
    &[],
    0,
    Rgba::BLACK,
    0.0,
    &[],
    None,
  );

  assert_eq!(
    FAILED_ALLOCS.load(Ordering::Relaxed),
    start_failures + 1,
    "expected to trigger gradient stop allocation failure"
  );
  assert!(
    rendered.is_none(),
    "expected color glyph rendering to return None after allocation failure"
  );
}

