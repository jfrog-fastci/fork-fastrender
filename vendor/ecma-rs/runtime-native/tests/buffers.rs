use core::mem::MaybeUninit;

use runtime_native::{ArrayBuffer, BackingStoreAllocator, GlobalBackingStoreAllocator, Uint8Array};

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
