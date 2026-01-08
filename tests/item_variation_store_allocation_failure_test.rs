use fastrender::text::otvar::item_variation_store::{
  parse_item_variation_store, ParseError, VariationRegion,
};
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
fn item_variation_store_allocation_failure_is_reported_instead_of_aborting() {
  let _guard = LOCK.lock().unwrap();

  let region_count: u16 = 12_345;
  let axis_count: u16 = 1;

  // ItemVariationStore header:
  // u16 format (1), u32 variationRegionListOffset, u16 itemVariationDataCount, u32 offsets[]
  let region_list_offset: u32 = 12;
  let region_list_bytes = 4u32 + u32::from(region_count) * 6;
  let item_data_offset: u32 = region_list_offset + region_list_bytes;

  // Minimal ItemVariationData: 1 item, 1 region index, one 8-bit delta.
  let item_data_bytes: usize = 9;
  let total_len = item_data_offset as usize + item_data_bytes;
  let mut data = vec![0u8; total_len];

  data[0..2].copy_from_slice(&1u16.to_be_bytes()); // format
  data[2..6].copy_from_slice(&region_list_offset.to_be_bytes());
  data[6..8].copy_from_slice(&1u16.to_be_bytes()); // itemVariationDataCount=1
  data[8..12].copy_from_slice(&item_data_offset.to_be_bytes());

  let region_start = region_list_offset as usize;
  data[region_start..region_start + 2].copy_from_slice(&axis_count.to_be_bytes());
  data[region_start + 2..region_start + 4].copy_from_slice(&region_count.to_be_bytes());
  // Leave axis records zeroed; they are only used for scalar computation, not parsing.

  let item_start = item_data_offset as usize;
  data[item_start..item_start + 2].copy_from_slice(&1u16.to_be_bytes()); // item_count
  data[item_start + 2..item_start + 4].copy_from_slice(&0u16.to_be_bytes()); // short_delta_count
  data[item_start + 4..item_start + 6].copy_from_slice(&1u16.to_be_bytes()); // region_index_count
  data[item_start + 6..item_start + 8].copy_from_slice(&0u16.to_be_bytes()); // region index 0
  data[item_start + 8] = 0; // delta

  let alloc_size = region_count as usize * mem::size_of::<VariationRegion>();
  let alloc_align = mem::align_of::<VariationRegion>();
  let start_failures = FAILED_ALLOCS.load(Ordering::Relaxed);
  fail_next_allocation(alloc_size, alloc_align);

  let result = parse_item_variation_store(&data);
  assert_eq!(
    FAILED_ALLOCS.load(Ordering::Relaxed),
    start_failures + 1,
    "expected to trigger region list allocation failure"
  );
  assert_eq!(
    result,
    Err(ParseError::InvalidValue(
      "variation region list allocation failed"
    ))
  );
}

