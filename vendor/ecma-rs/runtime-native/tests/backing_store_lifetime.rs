use runtime_native::buffer::{
  ArrayBuffer, ArrayBufferError, BackingStoreAllocator, GlobalBackingStoreAllocator,
};

#[test]
fn backing_store_survives_arraybuffer_finalization_while_pinned() {
  let alloc = GlobalBackingStoreAllocator::default();
  let mut buf = ArrayBuffer::new_zeroed_in(&alloc, 16).expect("alloc");
  assert_eq!(alloc.external_bytes(), 16);

  let pin = buf.pin().expect("pin backing store");
  assert_eq!(pin.len(), 16);
  assert_eq!(alloc.external_bytes(), 16);

  unsafe {
    let bytes = std::slice::from_raw_parts_mut(pin.as_ptr(), pin.len());
    bytes[0] = 0xAA;
    bytes[15] = 0xBB;
  }

  // Simulate the owning ArrayBuffer header being collected by the GC.
  buf.finalize_in(&alloc);

  // The backing store must remain alive while the host pin guard exists.
  assert_eq!(alloc.external_bytes(), 16);
  unsafe {
    let bytes = std::slice::from_raw_parts(pin.as_ptr(), pin.len());
    assert_eq!(bytes[0], 0xAA);
    assert_eq!(bytes[15], 0xBB);
  }

  drop(buf);
  assert_eq!(alloc.external_bytes(), 16);

  drop(pin);
  assert_eq!(alloc.external_bytes(), 0);
}

#[test]
fn detach_requires_not_pinned() {
  let alloc = GlobalBackingStoreAllocator::default();
  let mut buf = ArrayBuffer::new_zeroed_in(&alloc, 8).expect("alloc");
  let pin = buf.pin().expect("pin backing store");

  assert!(matches!(buf.detach(), Err(ArrayBufferError::Pinned)));

  drop(pin);
  buf.detach().unwrap();
  assert_eq!(buf.byte_len(), 0);
  assert!(buf.is_detached());
  assert_eq!(alloc.external_bytes(), 0);
}
