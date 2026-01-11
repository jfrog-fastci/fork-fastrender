use runtime_native::buffer::{ArrayBuffer, ArrayBufferError, TypedArrayError, Uint8Array};

#[test]
fn detach_while_pinned_is_rejected() {
  let mut buf = ArrayBuffer::new_zeroed(8).expect("alloc");
  let _pinned = buf.pin().expect("pin should succeed");

  assert_eq!(buf.detach(), Err(ArrayBufferError::Pinned));
  assert!(!buf.is_detached());
  assert_eq!(buf.byte_len(), 8);
}

#[test]
fn detach_after_unpin_detaches_and_views_observe_empty_semantics() {
  let mut buf = ArrayBuffer::from_boxed_slice(vec![1, 2, 3, 4].into_boxed_slice()).expect("alloc");
  let view = Uint8Array::view(&buf, 0, 4).expect("view should be in-bounds");

  {
    let _pinned = buf.pin().expect("pin should succeed");
    assert_eq!(buf.detach(), Err(ArrayBufferError::Pinned));
  }

  buf.detach().expect("detach after unpin should succeed");

  assert!(buf.is_detached());
  assert_eq!(buf.byte_len(), 0);

  assert!(view.is_detached());
  assert_eq!(view.length(), 0);
  assert_eq!(view.byte_length(), 0);
  assert_eq!(view.get(0).unwrap(), None);
  assert!(matches!(
    view.as_ptr_range(),
    Err(TypedArrayError::Buffer(ArrayBufferError::Detached))
  ));
}

#[test]
fn detach_is_idempotent() {
  let mut buf = ArrayBuffer::new_zeroed(1).expect("alloc");
  buf.detach().unwrap();
  buf.detach().unwrap();
  assert!(buf.is_detached());
  assert_eq!(buf.byte_len(), 0);
}

#[test]
fn transfer_moves_bytes_and_detaches_original() {
  let mut buf = ArrayBuffer::from_boxed_slice(vec![9, 8, 7].into_boxed_slice()).expect("alloc");
  let view = Uint8Array::view(&buf, 0, 3).unwrap();

  {
    let _pinned = buf.pin().expect("pin should succeed");
    assert!(matches!(buf.transfer(), Err(ArrayBufferError::Pinned)));
    assert_eq!(view.length(), 3);
  }

  let new_buf = buf.transfer().expect("transfer after unpin should succeed");

  assert!(buf.is_detached());
  assert_eq!(buf.byte_len(), 0);
  assert!(view.is_detached());
  assert_eq!(view.length(), 0);
  assert_eq!(view.get(0).unwrap(), None);

  assert!(!new_buf.is_detached());
  let pinned = new_buf.pin().unwrap();
  unsafe {
    assert_eq!(pinned.as_slice(), &[9, 8, 7]);
  }
}
