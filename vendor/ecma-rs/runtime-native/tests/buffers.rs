use core::mem::MaybeUninit;

use runtime_native::{ArrayBuffer, BackingStoreAllocator, GlobalBackingStoreAllocator, Uint8Array};
use runtime_native::{GcHeap, RootStack};
use runtime_native::gc::SimpleRememberedSet;
use runtime_native::test_util::TestRuntimeGuard;

#[test]
fn backing_store_pointer_is_stable_across_header_relocation() {
  let alloc = GlobalBackingStoreAllocator::default();

  let buf = ArrayBuffer::new_zeroed_in(&alloc, 64).expect("alloc");
  assert_eq!(alloc.external_bytes(), 64);
  let ptr_before = buf.data_ptr().expect("ptr");

  // Simulate a moving GC relocating the *header object* with a raw `memcpy`.
  let relocated = unsafe {
    let mut dst = MaybeUninit::<ArrayBuffer>::uninit();
    core::ptr::copy_nonoverlapping(
      &buf as *const ArrayBuffer as *const u8,
      dst.as_mut_ptr() as *mut u8,
      core::mem::size_of::<ArrayBuffer>(),
    );
    dst.assume_init()
  };

  // The old header becomes unreachable after relocation; it won't be finalized.
  core::mem::forget(buf);

  let mut relocated = relocated;
  assert_eq!(relocated.data_ptr().expect("ptr"), ptr_before);

  relocated.finalize_in(&alloc);
  assert_eq!(alloc.external_bytes(), 0);
}

#[test]
fn uint8array_view_is_bounds_checked() {
  let alloc = GlobalBackingStoreAllocator::default();
  let mut buf = ArrayBuffer::new_zeroed_in(&alloc, 8).expect("alloc");

  let view = Uint8Array::view(&buf, 2, 4).expect("in-bounds view");
  let (ptr, len) = view.as_ptr_range().expect("ptr range");
  assert_eq!(len, 4);
  assert_eq!(ptr, unsafe { buf.data_ptr().unwrap().add(2) });

  assert!(Uint8Array::view(&buf, 7, 2).is_err());
  assert!(Uint8Array::view(&buf, 9, 0).is_err());

  buf.finalize_in(&alloc);
  assert_eq!(alloc.external_bytes(), 0);
}

#[test]
fn external_backing_store_bytes_are_accounted_and_released() {
  let alloc = GlobalBackingStoreAllocator::default();
  assert_eq!(alloc.external_bytes(), 0);

  let mut a = ArrayBuffer::new_zeroed_in(&alloc, 10).expect("alloc");
  assert_eq!(alloc.external_bytes(), 10);

  let mut b = ArrayBuffer::new_zeroed_in(&alloc, 20).expect("alloc");
  assert_eq!(alloc.external_bytes(), 30);

  b.finalize_in(&alloc);
  assert_eq!(alloc.external_bytes(), 10);

  a.finalize_in(&alloc);
  assert_eq!(alloc.external_bytes(), 0);
}

#[test]
fn gc_finalizer_releases_arraybuffer_backing_store_on_minor_gc() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();

  let count = 8usize;
  let size = 1024 * 1024;
  for _ in 0..count {
    heap.alloc_array_buffer_young(size).expect("alloc array buffer");
  }
  assert_eq!(heap.external_bytes(), count * size);

  let mut roots = RootStack::new();
  let mut remembered = SimpleRememberedSet::new();
  heap.collect_minor(&mut roots, &mut remembered);
  assert_eq!(heap.external_bytes(), 0);
}

#[test]
fn gc_finalizer_not_called_on_promotion_and_runs_once() {
  let _rt = TestRuntimeGuard::new();
  let mut heap = GcHeap::new();

  let size = 256 * 1024;
  let mut root = heap.alloc_array_buffer_young(size).expect("alloc array buffer");
  assert!(heap.is_in_nursery(root));
  assert_eq!(heap.external_bytes(), size);

  let mut roots = RootStack::new();
  roots.push(&mut root as *mut *mut u8);
  let mut remembered = SimpleRememberedSet::new();

  heap.collect_minor(&mut roots, &mut remembered);
  assert!(
    !heap.is_in_nursery(root),
    "expected ArrayBuffer header to be promoted out of nursery"
  );
  assert_eq!(heap.external_bytes(), size);

  // Still reachable: major GC must not run the finalizer.
  heap.collect_major(&mut roots, &mut remembered);
  assert_eq!(heap.external_bytes(), size);

  // Unreachable: major GC must run the finalizer once.
  let mut empty_roots = RootStack::new();
  heap.collect_major(&mut empty_roots, &mut remembered);
  assert_eq!(heap.external_bytes(), 0);

  // Subsequent collections must not run it again.
  heap.collect_major(&mut empty_roots, &mut remembered);
  assert_eq!(heap.external_bytes(), 0);
}
