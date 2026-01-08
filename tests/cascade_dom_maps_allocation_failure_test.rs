use fastrender::css::types::StyleSheet;
use fastrender::dom::{DomNode, DomNodeType};
use fastrender::style::cascade::apply_style_set_with_media_target_and_imports_cached_with_deadline;
use fastrender::style::media::MediaContext;
use fastrender::style::style_set::StyleSet;
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

fn build_large_dom(node_count: usize) -> DomNode {
  // Build a shallow DOM (depth 2) to avoid deep recursion in cascade on debug builds.
  let child_count = node_count.saturating_sub(1);
  let mut children = Vec::with_capacity(child_count);
  for _ in 0..child_count {
    children.push(DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: String::new(),
        attributes: Vec::new(),
      },
      children: Vec::new(),
    });
  }
  DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: selectors::context::QuirksMode::NoQuirks,
    },
    children,
  }
}

#[test]
fn cascade_survives_dom_maps_allocation_failure() {
  let _guard = LOCK.lock().unwrap();

  // 1 root + 12_344 children -> 12_345 total nodes.
  let node_count = 12_345usize;
  let dom = build_large_dom(node_count);
  let style_set = StyleSet::from_document(StyleSheet::new());
  let media_ctx = MediaContext::screen(800.0, 600.0);

  // Fail the `tree_scope_prefixes: Vec<u32>` allocation inside `DomMaps::new`.
  let alloc_len = node_count + 1;
  let alloc_size = alloc_len * mem::size_of::<u32>();
  let alloc_align = mem::align_of::<u32>();
  let start_failures = FAILED_ALLOCS.load(Ordering::Relaxed);
  fail_next_allocation(alloc_size, alloc_align);

  let result = apply_style_set_with_media_target_and_imports_cached_with_deadline(
    &dom,
    &style_set,
    &media_ctx,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
  );

  assert_eq!(
    FAILED_ALLOCS.load(Ordering::Relaxed),
    start_failures + 1,
    "expected to trigger DomMaps allocation failure"
  );
  assert!(
    result.is_err(),
    "expected cascade to return an error (not abort) after allocation failure"
  );
}

