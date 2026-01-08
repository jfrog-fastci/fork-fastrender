use fastrender::css::parser::parse_stylesheet;
use fastrender::dom::{DomNode, DomNodeType};
use fastrender::style::cascade::apply_style_set_with_media_target_and_imports_cached_with_deadline;
use fastrender::style::media::MediaContext;
use fastrender::style::style_set::StyleSet;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

struct FailingAllocator;

static FAIL_SIZE: AtomicUsize = AtomicUsize::new(0);
static FAIL_ALIGN: AtomicUsize = AtomicUsize::new(0);
static FAILED_ALLOCS: AtomicUsize = AtomicUsize::new(0);
static RECORDED_ALLOC_SIZE: AtomicUsize = AtomicUsize::new(0);
static RECORDED_ALLOC_ALIGN: AtomicUsize = AtomicUsize::new(0);

fn fail_next_allocation(size: usize, align: usize) {
  FAIL_ALIGN.store(align, Ordering::Relaxed);
  FAIL_SIZE.store(size, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for FailingAllocator {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    if RECORDED_ALLOC_SIZE.load(Ordering::Relaxed) == 0 {
      RECORDED_ALLOC_ALIGN.store(layout.align(), Ordering::Relaxed);
      RECORDED_ALLOC_SIZE.store(layout.size(), Ordering::Relaxed);
    }
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
    if RECORDED_ALLOC_SIZE.load(Ordering::Relaxed) == 0 {
      RECORDED_ALLOC_ALIGN.store(layout.align(), Ordering::Relaxed);
      RECORDED_ALLOC_SIZE.store(layout.size(), Ordering::Relaxed);
    }
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
    if RECORDED_ALLOC_SIZE.load(Ordering::Relaxed) == 0 {
      RECORDED_ALLOC_ALIGN.store(layout.align(), Ordering::Relaxed);
      RECORDED_ALLOC_SIZE.store(new_size, Ordering::Relaxed);
    }
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

fn build_stylesheet_with_rules(rule_count: usize) -> fastrender::css::types::StyleSheet {
  let mut css = String::new();
  for idx in 0..rule_count {
    css.push_str(&format!(".c{idx} {{ color: rgb(1, 2, 3); }}\n"));
  }
  parse_stylesheet(&css).expect("stylesheet should parse")
}

fn rule_sets_content_allocation_layout(rule_count: usize) -> (usize, usize) {
  // `Vec<bool>` is bitpacked and its internal allocation strategy is an implementation detail.
  // Capture the actual allocation layout by running the same `try_reserve_exact` call used by
  // `RuleIndex::new`.
  RECORDED_ALLOC_SIZE.store(0, Ordering::Relaxed);
  RECORDED_ALLOC_ALIGN.store(0, Ordering::Relaxed);
  let mut probe: Vec<bool> = Vec::new();
  probe
    .try_reserve_exact(rule_count)
    .expect("probe allocation should succeed");
  let alloc_size = RECORDED_ALLOC_SIZE.load(Ordering::Relaxed);
  let alloc_align = RECORDED_ALLOC_ALIGN.load(Ordering::Relaxed);
  assert!(alloc_size != 0, "expected probe to perform an allocation");
  (alloc_size, alloc_align)
}

#[test]
fn cascade_survives_rule_index_allocation_failure() {
  let _guard = LOCK.lock().unwrap();

  let rule_count = 20_003usize;
  let stylesheet = build_stylesheet_with_rules(rule_count);
  let style_set = StyleSet::from_document(stylesheet);

  let dom = DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: selectors::context::QuirksMode::NoQuirks,
    },
    children: vec![DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: String::new(),
        attributes: Vec::new(),
      },
      children: Vec::new(),
    }],
  };

  let media_ctx = MediaContext::screen(800.0, 600.0);

  // Fail the `rule_sets_content: Vec<bool>` allocation inside `RuleIndex::new`.
  let (alloc_size, alloc_align) = rule_sets_content_allocation_layout(rule_count);
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
    "expected to trigger rule index allocation failure"
  );
  assert!(
    result.is_err(),
    "expected cascade to return an error (not abort) after allocation failure"
  );
}
