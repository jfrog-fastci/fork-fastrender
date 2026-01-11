use std::mem;
use std::ptr;
use std::thread;

use runtime_native::buffer::{
  ArrayBuffer, ArrayBufferError, BackingStoreAllocator, GlobalBackingStoreAllocator, Uint8Array,
};
use runtime_native::gc::ObjHeader;
use runtime_native::gc::RememberedSet;
use runtime_native::gc::RootStack;
use runtime_native::gc::SimpleRememberedSet;
use runtime_native::gc::TypeDescriptor;
use runtime_native::GcHeap;

#[repr(C)]
struct Node {
  header: ObjHeader,
  next: *mut u8,
}

static NODE_PTR_OFFSETS: [u32; 1] = [mem::offset_of!(Node, next) as u32];
static NODE_DESC: TypeDescriptor = TypeDescriptor::new(mem::size_of::<Node>(), &NODE_PTR_OFFSETS);

#[derive(Default)]
struct NullRememberedSet;

impl RememberedSet for NullRememberedSet {
  fn for_each_remembered_obj(&mut self, _f: &mut dyn FnMut(*mut u8)) {}
  fn clear(&mut self) {}
  fn on_promoted_object(&mut self, _obj: *mut u8, _has_young_refs: bool) {}
}

#[test]
fn pinned_object_address_is_stable_across_minor_and_major_gc() {
  let mut heap = GcHeap::new();

  let pinned = heap.alloc_pinned(&NODE_DESC);
  assert!(heap.is_in_los(pinned), "pinned objects must live in LOS");
  assert!(unsafe { &*(pinned as *const ObjHeader) }.is_pinned());

  let pinned_addr = pinned;
  let mut root_pinned = pinned;
  let mut roots = RootStack::new();
  roots.push(&mut root_pinned as *mut *mut u8);

  heap.collect_minor(&mut roots, &mut NullRememberedSet::default());
  assert_eq!(root_pinned, pinned_addr);
  #[cfg(any(debug_assertions, feature = "gc_debug"))]
  heap.verify_from_roots(&mut roots);

  heap.collect_major(&mut roots, &mut NullRememberedSet::default());
  assert_eq!(root_pinned, pinned_addr);
  #[cfg(any(debug_assertions, feature = "gc_debug"))]
  heap.verify_from_roots(&mut roots);
}

#[test]
fn pinned_objects_are_traced_and_compat_with_minor_evacuation() {
  let mut heap = GcHeap::new();

  let pinned = heap.alloc_pinned(&NODE_DESC);
  let young = heap.alloc_young(&NODE_DESC);

  unsafe {
    (*(pinned as *mut Node)).next = young;
    (*(young as *mut Node)).next = ptr::null_mut();
  }

  let mut root_pinned = pinned;
  let mut roots = RootStack::new();
  roots.push(&mut root_pinned as *mut *mut u8);

  // The pinned object now contains an old->young edge, which would normally be recorded by the
  // write barrier. For the test, we record it explicitly in a `SimpleRememberedSet`.
  let mut remembered = SimpleRememberedSet::new();
  remembered.on_promoted_object(pinned, true);
  assert!(remembered.contains(pinned));
  assert!(unsafe { &*(pinned as *const ObjHeader) }.is_remembered());
  heap.collect_minor(&mut roots, &mut remembered);

  assert_eq!(root_pinned, pinned);
  let updated = unsafe { (*(pinned as *mut Node)).next };
  assert_ne!(updated, young);
  assert!(!heap.is_in_nursery(updated));
  assert!(heap.is_in_immix(updated));
  assert!(!remembered.contains(pinned));
  assert!(!unsafe { &*(pinned as *const ObjHeader) }.is_remembered());
  assert!(unsafe { &*(pinned as *const ObjHeader) }.is_pinned());

  // Major GC should keep both pinned + its child alive.
  heap.collect_major(&mut roots, &mut NullRememberedSet::default());
  assert_eq!(unsafe { (*(pinned as *mut Node)).next }, updated);
  #[cfg(any(debug_assertions, feature = "gc_debug"))]
  heap.verify_from_roots(&mut roots);
}

#[test]
fn unreachable_pinned_objects_are_collectible() {
  let mut heap = GcHeap::new();
  assert_eq!(heap.los_object_count(), 0);

  let _pinned = heap.alloc_pinned(&NODE_DESC);
  assert_eq!(heap.los_object_count(), 1);

  let mut roots = RootStack::new();
  heap.collect_major(&mut roots, &mut NullRememberedSet::default());
  assert_eq!(heap.los_object_count(), 0);
}

#[test]
fn pin_unpin_toggles_pin_count() {
  let buffer = ArrayBuffer::new_zeroed(8).unwrap();
  assert_eq!(buffer.pin_count(), 0);

  let pinned = buffer.pin().unwrap();
  assert_eq!(pinned.len(), 8);
  assert_eq!(buffer.pin_count(), 1);

  drop(pinned);
  assert_eq!(buffer.pin_count(), 0);
}

#[test]
fn pin_is_raii_on_panic() {
  let buffer = ArrayBuffer::new_zeroed(4).unwrap();
  assert_eq!(buffer.pin_count(), 0);

  let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    let _pinned = buffer.pin().unwrap();
    assert_eq!(buffer.pin_count(), 1);
    panic!("boom");
  }));

  assert!(result.is_err());
  assert_eq!(buffer.pin_count(), 0);
}

#[test]
fn array_buffer_pin_range_returns_subslice() {
  let buffer = ArrayBuffer::from_bytes((0u8..8).collect::<Vec<u8>>()).unwrap();
  let base = buffer.data_ptr().unwrap();

  let pinned = buffer.pin_range(2..6).unwrap();
  assert_eq!(buffer.pin_count(), 1);
  assert_eq!(pinned.len(), 4);
  assert_eq!((pinned.as_ptr() as usize) - (base as usize), 2);
  // SAFETY: slice is valid while `pinned` is alive.
  unsafe {
    assert_eq!(pinned.as_slice(), &[2, 3, 4, 5]);
  }

  drop(pinned);
  assert_eq!(buffer.pin_count(), 0);
}

#[test]
fn uint8_array_pin_range_returns_subslice() {
  let buffer = ArrayBuffer::from_bytes((0u8..8).collect::<Vec<u8>>()).unwrap();
  let view = Uint8Array::view(&buffer, 2, 4).unwrap();

  let pinned = view.pin_range(1..3).unwrap();
  assert_eq!(buffer.pin_count(), 1);
  assert_eq!(pinned.len(), 2);

  let base = buffer.data_ptr().unwrap();
  assert_eq!((pinned.as_ptr() as usize) - (base as usize), 3);

  // SAFETY: slice is valid while `pinned` is alive.
  unsafe {
    assert_eq!(pinned.as_slice(), &[3, 4]);
  }

  drop(pinned);
  assert_eq!(buffer.pin_count(), 0);
}

#[test]
fn detach_transfer_and_resize_are_blocked_while_pinned() {
  let alloc = GlobalBackingStoreAllocator::default();
  let mut buffer = ArrayBuffer::new_zeroed_in(&alloc, 8).unwrap();
  assert_eq!(alloc.external_bytes(), 8);

  let pinned = buffer.pin().unwrap();
  assert_eq!(buffer.pin_count(), 1);

  assert_eq!(buffer.detach(), Err(ArrayBufferError::Pinned));
  assert_eq!(buffer.transfer().unwrap_err(), ArrayBufferError::Pinned);
  assert_eq!(buffer.resize(16), Err(ArrayBufferError::Pinned));
  assert_eq!(buffer.resize(4), Err(ArrayBufferError::Pinned));

  drop(pinned);
  assert_eq!(buffer.pin_count(), 0);

  buffer.detach().unwrap();
  assert!(buffer.is_detached());
  assert_eq!(buffer.byte_len(), 0);
  assert!(matches!(buffer.pin(), Err(ArrayBufferError::Detached)));
  assert_eq!(alloc.external_bytes(), 0);

  let transferred = buffer.transfer().unwrap();
  assert!(transferred.is_detached());
}

#[test]
fn pinned_array_buffer_can_drop_on_other_thread_after_finalize() {
  let alloc = GlobalBackingStoreAllocator::default();
  let mut buffer = ArrayBuffer::new_zeroed_in(&alloc, 8).unwrap();
  assert_eq!(alloc.external_bytes(), 8);

  let pinned = buffer.pin().unwrap();
  buffer.finalize_in(&alloc);
  assert!(buffer.is_detached());
  assert_eq!(alloc.external_bytes(), 8);

  let alloc_thread = alloc.clone();
  let handle = thread::spawn(move || {
    let mut pinned = pinned;
    assert_eq!(pinned.len(), 8);
    // SAFETY: slice is valid while `pinned` is alive.
    unsafe {
      pinned.as_mut_slice().fill(0xAA);
    }
    drop(pinned);
    assert_eq!(alloc_thread.external_bytes(), 0);
  });

  handle.join().unwrap();
  assert_eq!(alloc.external_bytes(), 0);
 
  // Detach is idempotent.
  buffer.detach().unwrap();
  assert!(matches!(buffer.pin(), Err(ArrayBufferError::Detached)));
}

#[test]
fn finalize_defers_free_until_last_unpin() {
  let alloc = GlobalBackingStoreAllocator::default();
  let mut buffer = ArrayBuffer::new_zeroed_in(&alloc, 8).unwrap();
  assert_eq!(alloc.external_bytes(), 8);

  let pinned = buffer.pin().unwrap();

  // Finalization must not free while pinned: the header becomes detached, but the backing store
  // stays alive until the last pin guard is dropped.
  buffer.finalize_in(&alloc);
  assert!(buffer.is_detached());
  assert_eq!(alloc.external_bytes(), 8);

  drop(pinned);
  assert_eq!(alloc.external_bytes(), 0);
}

#[test]
fn typed_array_pins_subrange() {
  let buffer = ArrayBuffer::new_zeroed(8).unwrap();
  let view = Uint8Array::view(&buffer, 2, 4).unwrap();
  assert_eq!(buffer.pin_count(), 0);

  let pinned_buf = buffer.pin().unwrap();
  let ptr_buf = pinned_buf.as_ptr();
  let len_buf = pinned_buf.len();
  assert_eq!(len_buf, 8);
  assert_eq!(buffer.pin_count(), 1);

  let pinned_view = view.pin().unwrap();
  let ptr_view = pinned_view.as_ptr();
  let len_view = pinned_view.len();
  assert_eq!(len_view, 4);
  assert_eq!((ptr_view as usize) - (ptr_buf as usize), 2);
  assert_eq!(buffer.pin_count(), 2);

  drop(pinned_view);
  assert_eq!(buffer.pin_count(), 1);
  drop(pinned_buf);
  assert_eq!(buffer.pin_count(), 0);
}
