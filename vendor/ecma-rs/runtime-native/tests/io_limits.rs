use runtime_native::buffer::{
  global_backing_store_allocator, ArrayBuffer, ArrayBufferError, BackingStoreAllocator, BorrowError,
};
use runtime_native::io::{IoLimitError, IoLimits, IoLimiter, IoOp, IoVecRange, PinnedIoVec};
use runtime_native::Uint8Array;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

#[test]
fn max_pinned_bytes_rejects_additional_pins() {
  let limiter = Arc::new(IoLimiter::new(IoLimits {
    max_pinned_bytes: 10,
    max_inflight_ops: 8,
    max_pinned_bytes_per_op: None,
  }));

  let buf_a = global_backing_store_allocator().alloc_zeroed(8).unwrap();
  let op_a = IoOp::pin_backing_store_range(&limiter, buf_a, 0..8).unwrap();
  assert_eq!(
    limiter.counters().pinned_bytes_current,
    8,
    "first op should be accounted"
  );

  let buf_b = global_backing_store_allocator().alloc_zeroed(8).unwrap();
  let err = IoOp::pin_backing_store_range(&limiter, buf_b, 0..8).unwrap_err();
  assert!(matches!(err, IoLimitError::LimitExceeded("max pinned bytes")));
  assert_eq!(
    limiter.counters().pinned_bytes_current,
    8,
    "rejected pins must not change counters"
  );

  drop(op_a);
  assert_eq!(limiter.counters().pinned_bytes_current, 0);
}

#[test]
fn counters_decrement_on_drop_and_allow_subsequent_ops() {
  let limiter = Arc::new(IoLimiter::new(IoLimits {
    max_pinned_bytes: 1024,
    max_inflight_ops: 1,
    max_pinned_bytes_per_op: None,
  }));

  let buf_a = global_backing_store_allocator().alloc_zeroed(4).unwrap();
  let op_a = IoOp::pin_backing_store_range(&limiter, buf_a, 0..4).unwrap();
  assert_eq!(limiter.counters().inflight_ops_current, 1);

  let buf_b = global_backing_store_allocator().alloc_zeroed(4).unwrap();
  let err = IoOp::pin_backing_store_range(&limiter, buf_b, 0..4).unwrap_err();
  assert!(matches!(err, IoLimitError::LimitExceeded("max inflight ops")));
  assert_eq!(limiter.counters().inflight_ops_current, 1);

  drop(op_a);
  assert_eq!(limiter.counters().inflight_ops_current, 0);

  let buf_c = global_backing_store_allocator().alloc_zeroed(4).unwrap();
  let _op_c = IoOp::pin_backing_store_range(&limiter, buf_c, 0..4).unwrap();
  assert_eq!(limiter.counters().inflight_ops_current, 1);
}

#[test]
fn no_counter_leaks_on_error_paths_or_panics() {
  let limiter = Arc::new(IoLimiter::new(IoLimits {
    max_pinned_bytes: 1024,
    max_inflight_ops: 8,
    max_pinned_bytes_per_op: None,
  }));

  // Invalid range should error without affecting accounting.
  let buf = global_backing_store_allocator().alloc_zeroed(4).unwrap();
  let _err = IoOp::pin_backing_store_range(&limiter, buf.clone(), 0..10).unwrap_err();
  assert_eq!(limiter.counters().pinned_bytes_current, 0);
  assert_eq!(limiter.counters().inflight_ops_current, 0);

  // Panics should not leak permits: drop during unwind must decrement counters.
  let res = std::panic::catch_unwind(AssertUnwindSafe(|| {
    let _op = IoOp::pin_backing_store_range(&limiter, buf, 0..4).unwrap();
    assert_eq!(limiter.counters().pinned_bytes_current, 4);
    assert_eq!(limiter.counters().inflight_ops_current, 1);
    panic!("boom");
  }));
  assert!(res.is_err());
  assert_eq!(limiter.counters().pinned_bytes_current, 0);
  assert_eq!(limiter.counters().inflight_ops_current, 0);
}

#[test]
fn vectored_io_charges_sum_of_backing_store_allocations() {
  let limiter_tight = Arc::new(IoLimiter::new(IoLimits {
    max_pinned_bytes: 7,
    max_inflight_ops: 8,
    max_pinned_bytes_per_op: None,
  }));

  let buf_a = global_backing_store_allocator().alloc_zeroed(4).unwrap();
  let buf_b = global_backing_store_allocator().alloc_zeroed(4).unwrap();

  // Each segment range retains the entire backing allocation. Total pinned bytes = 4 + 4 = 8.
  let err = IoOp::pin_vectored(&limiter_tight, vec![(buf_a, 0..4), (buf_b, 0..2)]).unwrap_err();
  assert!(matches!(err, IoLimitError::LimitExceeded("max pinned bytes")));
  assert_eq!(limiter_tight.counters().pinned_bytes_current, 0);
  assert_eq!(limiter_tight.counters().inflight_ops_current, 0);

  let limiter = Arc::new(IoLimiter::new(IoLimits {
    max_pinned_bytes: 8,
    max_inflight_ops: 8,
    max_pinned_bytes_per_op: None,
  }));

  let buf_a = global_backing_store_allocator().alloc_zeroed(4).unwrap();
  let buf_b = global_backing_store_allocator().alloc_zeroed(4).unwrap();
  let op = IoOp::pin_vectored(&limiter, vec![(buf_a, 0..3), (buf_b, 0..2)]).unwrap();
  assert_eq!(limiter.counters().pinned_bytes_current, 8);
  assert_eq!(limiter.counters().inflight_ops_current, 1);
  drop(op);
  assert_eq!(limiter.counters().pinned_bytes_current, 0);
  assert_eq!(limiter.counters().inflight_ops_current, 0);
}

#[test]
fn pinning_small_range_charges_full_backing_allocation_len() {
  let limiter = Arc::new(IoLimiter::new(IoLimits {
    max_pinned_bytes: 1024,
    max_inflight_ops: 8,
    max_pinned_bytes_per_op: None,
  }));

  let store = global_backing_store_allocator().alloc_zeroed(1024).unwrap();
  let op = IoOp::pin_backing_store_range(&limiter, store, 0..1).unwrap();
  assert_eq!(limiter.counters().pinned_bytes_current, 1024);
  drop(op);
  assert_eq!(limiter.counters().pinned_bytes_current, 0);
}

#[test]
fn vectored_io_dedupes_backing_stores_within_an_op() {
  let limiter = Arc::new(IoLimiter::new(IoLimits {
    max_pinned_bytes: 8,
    max_inflight_ops: 8,
    max_pinned_bytes_per_op: None,
  }));

  let store = global_backing_store_allocator().alloc_zeroed(8).unwrap();
  let op = IoOp::pin_vectored(
    &limiter,
    vec![(store.clone(), 0..1), (store.clone(), 1..2), (store, 2..3)],
  )
  .unwrap();
  assert_eq!(
    limiter.counters().pinned_bytes_current,
    8,
    "backing store must be charged once even if referenced multiple times"
  );
  drop(op);
  assert_eq!(limiter.counters().pinned_bytes_current, 0);
}

#[test]
fn array_buffer_detach_rejected_while_pinned_by_io_op() {
  let limiter = Arc::new(IoLimiter::new(IoLimits {
    max_pinned_bytes: 1024,
    max_inflight_ops: 8,
    max_pinned_bytes_per_op: None,
  }));

  let mut buf = ArrayBuffer::new_zeroed(16).unwrap();
  let op = IoOp::pin_array_buffer_range(&limiter, &buf, 0..1).unwrap();
  assert_eq!(buf.detach().unwrap_err(), ArrayBufferError::Pinned);
  drop(op);
  buf.detach().unwrap();
  assert!(buf.is_detached());
}

#[test]
fn pinning_charges_backing_store_alloc_len_even_when_byte_len_is_smaller() {
  let limiter = Arc::new(IoLimiter::new(IoLimits {
    max_pinned_bytes: 1024,
    max_inflight_ops: 8,
    max_pinned_bytes_per_op: None,
  }));

  // Use an adopted Vec with capacity > len so `alloc_len()` differs from `byte_len()`.
  // This defends against accidental changes that start charging only the visible byteLength.
  let bytes = {
    let mut chosen = None;
    for _ in 0..32 {
      let mut v = Vec::with_capacity(1024);
      v.push(0u8);
      if (v.as_ptr() as usize) % runtime_native::buffer::BACKING_STORE_MIN_ALIGN == 0 {
        chosen = Some(v);
        break;
      }
    }
    chosen.expect("failed to allocate a Vec<u8> with min backing store alignment")
  };

  let buf = ArrayBuffer::from_bytes(bytes).unwrap();
  assert_eq!(buf.byte_len(), 1);
  let alloc_len = buf.backing_store_handle().unwrap().alloc_len();
  assert_eq!(alloc_len, 1024);

  let op = IoOp::pin_array_buffer_range(&limiter, &buf, 0..1).unwrap();
  assert_eq!(limiter.counters().pinned_bytes_current, alloc_len);
  drop(op);
  assert_eq!(limiter.counters().pinned_bytes_current, 0);
}

#[test]
fn pin_iovecs_charges_deduped_alloc_len_per_backing_store() {
  let limiter = Arc::new(IoLimiter::new(IoLimits {
    max_pinned_bytes: 8,
    max_inflight_ops: 8,
    max_pinned_bytes_per_op: None,
  }));

  let buf = ArrayBuffer::new_zeroed(8).unwrap();

  // Two segments, same backing store: should be charged once (8), not twice (16) and not by the
  // segment lengths (2).
  let ranges = [
    IoVecRange::array_buffer(&buf, 0, 1).unwrap(),
    IoVecRange::array_buffer(&buf, 1, 1).unwrap(),
  ];
  let iovecs = PinnedIoVec::try_from_ranges(&ranges).unwrap();
  let op = IoOp::pin_iovecs(&limiter, iovecs).unwrap();
  assert_eq!(limiter.counters().pinned_bytes_current, 8);
  drop(op);
  assert_eq!(limiter.counters().pinned_bytes_current, 0);
}

#[test]
fn pin_uint8_array_range_converts_view_relative_range_and_charges_alloc_len() {
  let limiter = Arc::new(IoLimiter::new(IoLimits {
    max_pinned_bytes: 2048,
    max_inflight_ops: 8,
    max_pinned_bytes_per_op: None,
  }));

  // Adopt an aligned Vec with capacity > len so `alloc_len()` differs from `byte_len()`.
  let bytes = {
    let mut chosen = None;
    for _ in 0..64 {
      let mut v = Vec::with_capacity(1024);
      v.extend_from_slice(&[0u8; 16]);
      if (v.as_ptr() as usize) % runtime_native::buffer::BACKING_STORE_MIN_ALIGN == 0 {
        chosen = Some(v);
        break;
      }
    }
    chosen.expect("failed to allocate a Vec<u8> with min backing store alignment")
  };

  let buf = ArrayBuffer::from_bytes(bytes).unwrap();
  assert_eq!(buf.byte_len(), 16);
  let alloc_len = buf.backing_store_handle().unwrap().alloc_len();
  assert_eq!(alloc_len, 1024);

  let view = Uint8Array::view(&buf, 4, 8).unwrap();
  // `IoOp::pin_uint8_array_range` borrows the backing store for the lifetime of the op; once pinned,
  // `ArrayBuffer::data_ptr()` must be rejected. Compute the expected base pointer before pinning.
  let expected_ptr = unsafe { buf.data_ptr().unwrap().add(4 + 2) } as *const u8;
  let op = IoOp::pin_uint8_array_range(&limiter, &view, 2..6).unwrap();

  assert_eq!(limiter.counters().pinned_bytes_current, alloc_len);
  assert_eq!(op.bufs().len(), 1);
  assert_eq!(op.bufs()[0].len(), 4);
  assert_eq!(op.bufs()[0].as_ptr(), expected_ptr);

  drop(op);
  assert_eq!(limiter.counters().pinned_bytes_current, 0);
}

#[test]
fn pin_array_buffer_range_produces_expected_kernel_ptr() {
  let limiter = Arc::new(IoLimiter::new(IoLimits {
    max_pinned_bytes: 16,
    max_inflight_ops: 8,
    max_pinned_bytes_per_op: None,
  }));

  let buf = ArrayBuffer::new_zeroed(16).unwrap();
  // `IoOp::pin_array_buffer_range` borrows the backing store for the lifetime of the op; compute the
  // expected stable base pointer before pinning.
  let expected_ptr = unsafe { buf.data_ptr().unwrap().add(4) } as *const u8;
  let op = IoOp::pin_array_buffer_range(&limiter, &buf, 4..10).unwrap();

  assert_eq!(limiter.counters().pinned_bytes_current, 16);
  assert_eq!(op.bufs().len(), 1);
  assert_eq!(op.bufs()[0].len(), 6);
  assert_eq!(op.bufs()[0].as_ptr(), expected_ptr);
  drop(op);

  assert_eq!(limiter.counters().pinned_bytes_current, 0);
}

#[test]
fn pin_backing_store_range_produces_expected_kernel_ptr() {
  let limiter = Arc::new(IoLimiter::new(IoLimits {
    max_pinned_bytes: 16,
    max_inflight_ops: 8,
    max_pinned_bytes_per_op: None,
  }));

  let store = global_backing_store_allocator().alloc_zeroed(16).unwrap();
  let alloc_len = store.alloc_len();
  let expected_ptr = unsafe { store.as_ptr().add(5) } as *const u8;

  let op = IoOp::pin_backing_store_range(&limiter, store, 5..9).unwrap();

  assert_eq!(limiter.counters().pinned_bytes_current, alloc_len);
  assert_eq!(op.bufs().len(), 1);
  assert_eq!(op.bufs()[0].len(), 4);
  assert_eq!(op.bufs()[0].as_ptr(), expected_ptr);

  drop(op);
  assert_eq!(limiter.counters().pinned_bytes_current, 0);
}

#[test]
fn pin_vectored_produces_expected_kernel_ptrs() {
  let limiter = Arc::new(IoLimiter::new(IoLimits {
    max_pinned_bytes: 24,
    max_inflight_ops: 8,
    max_pinned_bytes_per_op: None,
  }));

  let store_a = global_backing_store_allocator().alloc_zeroed(8).unwrap();
  let store_b = global_backing_store_allocator().alloc_zeroed(16).unwrap();

  let base_a = store_a.as_ptr() as *const u8;
  let base_b = store_b.as_ptr() as *const u8;

  let op = IoOp::pin_vectored(
    &limiter,
    vec![
      (store_a.clone(), 1..3),
      (store_b.clone(), 4..10),
      (store_a, 0..1),
    ],
  )
  .unwrap();

  assert_eq!(limiter.counters().pinned_bytes_current, 24);
  assert_eq!(op.bufs().len(), 3);

  assert_eq!(op.bufs()[0].len(), 2);
  assert_eq!(op.bufs()[0].as_ptr(), unsafe { base_a.add(1) });
  assert_eq!(op.bufs()[1].len(), 6);
  assert_eq!(op.bufs()[1].as_ptr(), unsafe { base_b.add(4) });
  assert_eq!(op.bufs()[2].len(), 1);
  assert_eq!(op.bufs()[2].as_ptr(), base_a);

  drop(op);
  assert_eq!(limiter.counters().pinned_bytes_current, 0);
}

#[test]
fn io_op_borrow_blocks_safe_buffer_access_and_is_released_on_drop() {
  let limiter = Arc::new(IoLimiter::new(IoLimits {
    max_pinned_bytes: 16,
    max_inflight_ops: 8,
    max_pinned_bytes_per_op: None,
  }));

  let mut buf = ArrayBuffer::new_zeroed(16).unwrap();
  assert!(buf.data_ptr().is_ok());

  let op = IoOp::pin_array_buffer_range(&limiter, &buf, 0..1).unwrap();

  assert!(matches!(
    buf.data_ptr(),
    Err(ArrayBufferError::Borrow(BorrowError::Borrowed))
  ));
  assert!(matches!(
    buf.try_with_slice(|_| ()),
    Err(ArrayBufferError::Borrow(BorrowError::Borrowed))
  ));
  assert!(matches!(
    buf.try_with_slice_mut(|_| ()),
    Err(ArrayBufferError::Borrow(BorrowError::Borrowed))
  ));

  drop(op);

  assert!(buf.data_ptr().is_ok());
  assert!(buf.try_with_slice(|_| ()).is_ok());
  assert!(buf.try_with_slice_mut(|_| ()).is_ok());
}

#[test]
fn borrowed_buffer_rejects_io_op_without_leaking_counters() {
  let limiter = Arc::new(IoLimiter::new(IoLimits {
    max_pinned_bytes: 16,
    max_inflight_ops: 8,
    max_pinned_bytes_per_op: None,
  }));

  let buf = ArrayBuffer::new_zeroed(16).unwrap();
  let _borrow = buf.try_borrow_io_write().unwrap();

  let err = IoOp::pin_array_buffer_range(&limiter, &buf, 0..1).unwrap_err();
  assert_eq!(err, IoLimitError::BufferBorrowed);
  assert_eq!(
    limiter.counters().pinned_bytes_current,
    0,
    "buffer borrow failures must not leak pinned-bytes accounting"
  );
  assert_eq!(
    limiter.counters().inflight_ops_current,
    0,
    "buffer borrow failures must not leak inflight-op accounting"
  );
}

#[test]
fn pins_block_detach_and_transfer_until_drop() {
  let limiter = Arc::new(IoLimiter::new(IoLimits {
    max_pinned_bytes: 1024,
    max_inflight_ops: 8,
    max_pinned_bytes_per_op: None,
  }));

  let mut buf = ArrayBuffer::new_zeroed(8).unwrap();
  let alloc_len = buf.backing_store_handle().unwrap().alloc_len();
  let view = Uint8Array::view(&buf, 2, 4).unwrap();

  let op = IoOp::pin_uint8_array(&limiter, &view).unwrap();
  assert_eq!(limiter.counters().pinned_bytes_current, alloc_len);
  assert_eq!(limiter.counters().inflight_ops_current, 1);

  assert!(matches!(buf.detach(), Err(ArrayBufferError::Pinned)));
  assert!(matches!(buf.transfer(), Err(ArrayBufferError::Pinned)));
  assert_eq!(buf.resize(16), Err(ArrayBufferError::Pinned));

  drop(op);
  assert_eq!(limiter.counters().pinned_bytes_current, 0);
  assert_eq!(limiter.counters().inflight_ops_current, 0);

  // After dropping the IoOp guard, detach/transfer must be allowed again.
  let _transferred = buf.transfer().unwrap();
  assert!(buf.is_detached());
}

#[test]
fn detached_buffers_return_buffer_not_alive() {
  let limiter = Arc::new(IoLimiter::new(IoLimits::default()));

  let mut buf = ArrayBuffer::new_zeroed(4).unwrap();
  let view = Uint8Array::view(&buf, 0, 4).unwrap();
  buf.detach().unwrap();

  assert!(matches!(
    IoOp::pin_array_buffer_range(&limiter, &buf, 0..0).unwrap_err(),
    IoLimitError::BufferNotAlive
  ));
  assert!(matches!(
    IoOp::pin_uint8_array(&limiter, &view).unwrap_err(),
    IoLimitError::BufferNotAlive
  ));
  assert_eq!(limiter.counters().pinned_bytes_current, 0);
  assert_eq!(limiter.counters().inflight_ops_current, 0);
}

#[test]
fn io_op_is_send() {
  fn assert_send<T: Send>() {}
  assert_send::<IoOp>();
}
