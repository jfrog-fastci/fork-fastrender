use fastrender::error::{Error, RenderError};
use fastrender::image_loader::ImageCache;
use fastrender::style::types::OrientationTransform;
use std::alloc::{GlobalAlloc, Layout, System};
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
fn about_url_placeholder_pixmap_survives_allocation_failure() {
  let _guard = LOCK.lock().unwrap();

  let cache = ImageCache::new();
  let start_failures = FAILED_ALLOCS.load(Ordering::Relaxed);

  // Fail the `Vec<u8>` allocation for the 1×1 pixmap buffer (4 RGBA bytes).
  fail_next_allocation(4, 1);

  let result = cache.load_raster_pixmap("about:blank", OrientationTransform::IDENTITY, false);

  assert_eq!(
    FAILED_ALLOCS.load(Ordering::Relaxed),
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

