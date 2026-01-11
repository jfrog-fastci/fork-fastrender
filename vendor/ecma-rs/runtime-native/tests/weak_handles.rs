use std::mem;

use runtime_native::gc::ObjHeader;
use runtime_native::gc::RememberedSet;
use runtime_native::gc::RootStack;
use runtime_native::gc::TypeDescriptor;
use runtime_native::GcHeap;
use runtime_native::test_util::TestRuntimeGuard;

static NO_PTR_OFFSETS: [u32; 0] = [];

#[repr(C)]
struct BoxedUsize {
  header: ObjHeader,
  value: usize,
}

static BOXED_USIZE_DESC: TypeDescriptor =
  TypeDescriptor::new(mem::size_of::<BoxedUsize>(), &NO_PTR_OFFSETS);

#[derive(Default)]
struct NullRememberedSet;

impl RememberedSet for NullRememberedSet {
  fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}
  fn clear(&mut self) {}
  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

#[test]
fn weak_handle_clears_on_minor_gc_when_unreachable() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();

  let obj = heap.alloc_young(&BOXED_USIZE_DESC);
  let handle = heap.weak_add(obj);

  let mut roots = RootStack::new();
  heap.collect_minor(&mut roots, &mut NullRememberedSet::default()).unwrap();

  assert_eq!(heap.weak_get(handle), None);
}

#[test]
fn weak_handle_updates_on_minor_gc_when_reachable() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();

  let obj = heap.alloc_young(&BOXED_USIZE_DESC);
  let handle = heap.weak_add(obj);

  let mut root_obj = obj;
  let mut roots = RootStack::new();
  roots.push(&mut root_obj as *mut *mut u8);

  heap.collect_minor(&mut roots, &mut NullRememberedSet::default()).unwrap();

  assert!(!heap.is_in_nursery(root_obj));
  assert_ne!(root_obj, obj);
  assert_eq!(heap.weak_get(handle), Some(root_obj));
}

#[test]
fn weak_handle_clears_on_major_gc_when_unreachable() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();

  let obj = heap.alloc_old(&BOXED_USIZE_DESC);
  let handle = heap.weak_add(obj);

  let mut roots = RootStack::new();
  heap.collect_major(&mut roots, &mut NullRememberedSet::default()).unwrap();

  assert_eq!(heap.weak_get(handle), None);
}

#[test]
fn weak_handle_survives_major_gc_when_reachable() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();

  let obj = heap.alloc_old(&BOXED_USIZE_DESC);
  let handle = heap.weak_add(obj);

  let mut root_obj = obj;
  let mut roots = RootStack::new();
  roots.push(&mut root_obj as *mut *mut u8);

  heap.collect_major(&mut roots, &mut NullRememberedSet::default()).unwrap();

  assert_eq!(root_obj, obj);
  assert_eq!(heap.weak_get(handle), Some(obj));
}

#[test]
fn weak_handle_remove_reuses_slots_with_generation_bumps() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();

  let a = heap.alloc_old(&BOXED_USIZE_DESC);
  let ha = heap.weak_add(a);
  heap.weak_remove(ha);

  let b = heap.alloc_old(&BOXED_USIZE_DESC);
  let hb = heap.weak_add(b);

  assert_ne!(ha, hb);
  assert_eq!(heap.weak_get(ha), None);
  assert_eq!(heap.weak_get(hb), Some(b));
}

struct WeakHandleGuard(u64);

impl WeakHandleGuard {
  fn new(ptr: *mut u8) -> Self {
    Self(runtime_native::rt_weak_add(ptr))
  }

  fn get(&self) -> *mut u8 {
    runtime_native::rt_weak_get(self.0)
  }
}

impl Drop for WeakHandleGuard {
  fn drop(&mut self) {
    runtime_native::rt_weak_remove(self.0);
  }
}

#[test]
fn abi_weak_handle_clears_on_minor_gc_when_unreachable() {
  let mut heap = GcHeap::new();

  let obj = heap.alloc_young(&BOXED_USIZE_DESC);
  let handle = WeakHandleGuard::new(obj);

  let mut roots = RootStack::new();
  heap
    .collect_minor(&mut roots, &mut NullRememberedSet::default())
    .unwrap();

  assert!(handle.get().is_null());
}

#[test]
fn abi_weak_handle_updates_on_minor_gc_when_reachable() {
  let mut heap = GcHeap::new();

  let obj = heap.alloc_young(&BOXED_USIZE_DESC);
  let handle = WeakHandleGuard::new(obj);

  let mut root_obj = obj;
  let mut roots = RootStack::new();
  roots.push(&mut root_obj as *mut *mut u8);

  heap
    .collect_minor(&mut roots, &mut NullRememberedSet::default())
    .unwrap();

  assert!(!heap.is_in_nursery(root_obj));
  assert_eq!(handle.get(), root_obj);
}

#[test]
fn abi_weak_handle_clears_on_major_gc_when_unreachable() {
  let mut heap = GcHeap::new();

  let obj = heap.alloc_old(&BOXED_USIZE_DESC);
  let handle = WeakHandleGuard::new(obj);

  let mut roots = RootStack::new();
  heap
    .collect_major(&mut roots, &mut NullRememberedSet::default())
    .unwrap();

  assert!(handle.get().is_null());
}
