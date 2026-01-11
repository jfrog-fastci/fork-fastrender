use runtime_native::buffer::{
  ArrayBuffer, ArrayBufferError, BackingStoreAllocator, BorrowError, GlobalBackingStoreAllocator,
};
use runtime_native::io::{IoLimits, IoLimiter, IoOp, IoVecRange, PinnedIoVec};
use std::sync::Arc;

#[test]
fn in_flight_borrow_blocks_slice_access() {
  let mut buf = ArrayBuffer::new_zeroed(8).unwrap();

  // Not borrowed yet.
  buf.try_with_slice(|s| assert_eq!(s.len(), 8)).unwrap();
  buf.try_with_slice_mut(|s| s[0] = 1).unwrap();

  // In-flight borrow blocks both immutable and mutable access.
  let borrow = buf.try_borrow_io_write().unwrap();
  assert!(buf.is_io_borrowed());
  assert_eq!(
    buf.data_ptr().unwrap_err(),
    ArrayBufferError::Borrow(BorrowError::Borrowed)
  );
  assert_eq!(
    buf.try_with_slice(|_| ()).unwrap_err(),
    ArrayBufferError::Borrow(BorrowError::Borrowed)
  );
  assert_eq!(
    buf.try_with_slice_mut(|_| ()).unwrap_err(),
    ArrayBufferError::Borrow(BorrowError::Borrowed)
  );

  drop(borrow);
  assert!(!buf.is_io_borrowed());
  buf.try_with_slice(|_| ()).unwrap();
  buf.try_with_slice_mut(|_| ()).unwrap();
}

#[test]
fn io_op_holds_borrow_until_drop() {
  let limiter = Arc::new(IoLimiter::new(IoLimits::default()));
  let mut buf = ArrayBuffer::new_zeroed(8).unwrap();

  {
    let iovecs = PinnedIoVec::try_from_ranges(&[IoVecRange::whole_array_buffer(&buf)]).unwrap();
    let _op = IoOp::pin_iovecs(&limiter, iovecs).expect("pin ArrayBuffer range");
    assert!(buf.is_io_borrowed());
    assert_eq!(
      buf.try_with_slice(|_| ()).unwrap_err(),
      ArrayBufferError::Borrow(BorrowError::Borrowed)
    );
    assert_eq!(
      buf.try_with_slice_mut(|_| ()).unwrap_err(),
      ArrayBufferError::Borrow(BorrowError::Borrowed)
    );
  }

  assert!(!buf.is_io_borrowed());
  buf.try_with_slice(|_| ()).unwrap();
}

#[test]
fn write_borrow_exclusive_across_io_ops() {
  let buf = ArrayBuffer::new_zeroed(1).unwrap();

  let _read1 = buf.try_borrow_io_read().unwrap();
  let _read2 = buf.try_borrow_io_read().unwrap();
  assert_eq!(
    buf.try_borrow_io_write().err().unwrap(),
    ArrayBufferError::Borrow(BorrowError::Borrowed)
  );
  // While any I/O borrow is active, slice access is blocked.
  assert_eq!(
    buf.try_with_slice(|_| ()).unwrap_err(),
    ArrayBufferError::Borrow(BorrowError::Borrowed)
  );
}

#[test]
fn free_pending_waits_for_borrow_release() {
  let alloc = GlobalBackingStoreAllocator::default();
  let mut buf = ArrayBuffer::new_zeroed_in(&alloc, 4).unwrap();

  assert_eq!(alloc.external_bytes(), 4);

  let borrow = buf.try_borrow_io_write().unwrap();
  buf.finalize_in(&alloc);
  assert_eq!(
    alloc.external_bytes(),
    4,
    "finalize should be delayed while an I/O borrow is active"
  );

  drop(borrow);
  assert_eq!(alloc.external_bytes(), 0);
}
