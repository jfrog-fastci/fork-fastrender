use once_cell::sync::Lazy;
use parking_lot::Mutex;
use runtime_native::array::RT_ARRAY_ELEM_PTR_FLAG;
use runtime_native::gc::{ObjHeader, RememberedSet, RootStack, CARD_TABLE_MIN_BYTES};
use runtime_native::GcHeap;

/// `GcHeap` instances currently share some process-global GC state (e.g. card table / GC-in-progress
/// invariants). Running multiple independent `GcHeap` allocations/collections concurrently in the
/// same process can therefore trigger intermittent aborts.
///
/// Rust integration tests run in parallel threads by default (`RUST_TEST_THREADS`), so serialize
/// these tests to keep them deterministic.
static TEST_MUTEX: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

#[derive(Default)]
struct NullRememberedSet;

impl RememberedSet for NullRememberedSet {
  fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}
  fn clear(&mut self) {}
  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

fn card_table_ptr(obj: *mut u8) -> *mut std::sync::atomic::AtomicU64 {
  // SAFETY: `obj` is expected to be a valid object base pointer.
  unsafe { (&*(obj as *const ObjHeader)).card_table_ptr() }
}

#[test]
fn promoted_large_pointer_array_gets_card_table() {
  let _guard = TEST_MUTEX.lock();
  let mut heap = GcHeap::new();

  let ptr_size = core::mem::size_of::<*mut u8>();
  let len = CARD_TABLE_MIN_BYTES.div_ceil(ptr_size);
  let elem_size = RT_ARRAY_ELEM_PTR_FLAG | ptr_size;

  let array = heap.alloc_array_young(len, elem_size);
  assert!(heap.is_in_nursery(array));
  assert!(card_table_ptr(array).is_null(), "young arrays must not have card tables");

  let mut root = array;
  let mut roots = RootStack::new();
  roots.push(&mut root as *mut *mut u8);
  heap
    .collect_minor(&mut roots, &mut NullRememberedSet::default())
    .unwrap();

  assert!(!heap.is_in_nursery(root), "array should have been promoted");
  assert!(
    !card_table_ptr(root).is_null(),
    "promoted large pointer array should have a card table"
  );
}

#[test]
fn promoted_small_pointer_array_does_not_get_card_table() {
  let _guard = TEST_MUTEX.lock();
  let mut heap = GcHeap::new();

  let ptr_size = core::mem::size_of::<*mut u8>();
  let min_len = CARD_TABLE_MIN_BYTES.div_ceil(ptr_size);
  let len = min_len.saturating_sub(1);
  let elem_size = RT_ARRAY_ELEM_PTR_FLAG | ptr_size;

  let array = heap.alloc_array_young(len, elem_size);

  let mut root = array;
  let mut roots = RootStack::new();
  roots.push(&mut root as *mut *mut u8);
  heap
    .collect_minor(&mut roots, &mut NullRememberedSet::default())
    .unwrap();

  assert!(
    card_table_ptr(root).is_null(),
    "small pointer arrays should not get card tables"
  );
}

#[test]
fn promoted_non_pointer_array_does_not_get_card_table() {
  let _guard = TEST_MUTEX.lock();
  let mut heap = GcHeap::new();

  // Use pointer-sized raw elements (no RT_ARRAY_ELEM_PTR_FLAG) so the payload
  // is large enough but must not be treated as pointers.
  let ptr_size = core::mem::size_of::<*mut u8>();
  let len = CARD_TABLE_MIN_BYTES.div_ceil(ptr_size);
  let elem_size = ptr_size;

  let array = heap.alloc_array_young(len, elem_size);

  let mut root = array;
  let mut roots = RootStack::new();
  roots.push(&mut root as *mut *mut u8);
  heap
    .collect_minor(&mut roots, &mut NullRememberedSet::default())
    .unwrap();

  assert!(
    card_table_ptr(root).is_null(),
    "non-pointer arrays should not get card tables"
  );
}

#[test]
fn old_large_pointer_array_gets_card_table() {
  let _guard = TEST_MUTEX.lock();
  let mut heap = GcHeap::new();

  let ptr_size = core::mem::size_of::<*mut u8>();
  let len = CARD_TABLE_MIN_BYTES.div_ceil(ptr_size);
  let elem_size = RT_ARRAY_ELEM_PTR_FLAG | ptr_size;

  let array = heap.alloc_array_old(len, elem_size);
  assert!(
    !card_table_ptr(array).is_null(),
    "old large pointer arrays should get card tables"
  );
}
