use fastrender::style::color::Rgba;
use fastrender::text::color_fonts::parse_cpal_palette;
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
fn cpal_palette_parse_survives_allocation_failure() {
  let _guard = LOCK.lock().unwrap();

  let num_entries: u16 = 12_345;
  let num_palettes: u16 = 1;
  let num_color_records: u16 = num_entries;

  // CPAL header:
  // version=0, numEntries, numPalettes, numColorRecords, colorOffset
  let color_offset: u32 = 14; // header (12) + palette index (2)
  let mut data = Vec::new();
  data.extend_from_slice(&0u16.to_be_bytes()); // version
  data.extend_from_slice(&num_entries.to_be_bytes());
  data.extend_from_slice(&num_palettes.to_be_bytes());
  data.extend_from_slice(&num_color_records.to_be_bytes());
  data.extend_from_slice(&color_offset.to_be_bytes());
  data.extend_from_slice(&0u16.to_be_bytes()); // palette index start

  // Pad to color offset and append BGRA records.
  data.resize(color_offset as usize, 0);
  data.resize(
    color_offset as usize + num_color_records as usize * 4,
    0xFF,
  );

  let parsed = parse_cpal_palette(&data, 0).expect("expected palette parse to succeed");
  assert_eq!(parsed.colors.len(), num_entries as usize);

  let alloc_size = num_entries as usize * mem::size_of::<Rgba>();
  let alloc_align = mem::align_of::<Rgba>();
  let start_failures = FAILED_ALLOCS.load(Ordering::Relaxed);
  fail_next_allocation(alloc_size, alloc_align);

  let parsed = parse_cpal_palette(&data, 0);
  assert_eq!(
    FAILED_ALLOCS.load(Ordering::Relaxed),
    start_failures + 1,
    "expected to trigger palette allocation failure"
  );
  assert!(
    parsed.is_none(),
    "expected palette parsing to return None after allocation failure"
  );
}

