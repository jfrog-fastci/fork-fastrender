use runtime_native::buffer::{global_backing_store_allocator, ArrayBuffer, ArrayBufferError, BackingStoreAllocator};
use runtime_native::io::{IoLimitError, IoLimits, IoLimiter, IoOp};
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
