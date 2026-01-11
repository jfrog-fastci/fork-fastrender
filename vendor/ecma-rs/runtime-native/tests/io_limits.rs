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

  let buf_a: Arc<[u8]> = Arc::from(vec![0u8; 8]);
  let op_a = IoOp::pin_range(&limiter, buf_a, 0..8).unwrap();
  assert_eq!(
    limiter.counters().pinned_bytes_current,
    8,
    "first op should be accounted"
  );

  let buf_b: Arc<[u8]> = Arc::from(vec![0u8; 8]);
  let err = IoOp::pin_range(&limiter, buf_b, 0..8).unwrap_err();
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

  let buf_a: Arc<[u8]> = Arc::from(vec![0u8; 4]);
  let op_a = IoOp::pin_range(&limiter, buf_a, 0..4).unwrap();
  assert_eq!(limiter.counters().inflight_ops_current, 1);

  let buf_b: Arc<[u8]> = Arc::from(vec![0u8; 4]);
  let err = IoOp::pin_range(&limiter, buf_b, 0..4).unwrap_err();
  assert!(matches!(err, IoLimitError::LimitExceeded("max inflight ops")));
  assert_eq!(limiter.counters().inflight_ops_current, 1);

  drop(op_a);
  assert_eq!(limiter.counters().inflight_ops_current, 0);

  let buf_c: Arc<[u8]> = Arc::from(vec![0u8; 4]);
  let _op_c = IoOp::pin_range(&limiter, buf_c, 0..4).unwrap();
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
  let buf: Arc<[u8]> = Arc::from(vec![0u8; 4]);
  let _err = IoOp::pin_range(&limiter, buf.clone(), 0..10).unwrap_err();
  assert_eq!(limiter.counters().pinned_bytes_current, 0);
  assert_eq!(limiter.counters().inflight_ops_current, 0);

  // Panics should not leak permits: drop during unwind must decrement counters.
  let res = std::panic::catch_unwind(AssertUnwindSafe(|| {
    let _op = IoOp::pin_range(&limiter, buf, 0..4).unwrap();
    assert_eq!(limiter.counters().pinned_bytes_current, 4);
    assert_eq!(limiter.counters().inflight_ops_current, 1);
    panic!("boom");
  }));
  assert!(res.is_err());
  assert_eq!(limiter.counters().pinned_bytes_current, 0);
  assert_eq!(limiter.counters().inflight_ops_current, 0);
}

#[test]
fn vectored_io_charges_sum_of_ranges() {
  let limiter = Arc::new(IoLimiter::new(IoLimits {
    max_pinned_bytes: 5,
    max_inflight_ops: 8,
    max_pinned_bytes_per_op: None,
  }));

  let buf_a: Arc<[u8]> = Arc::from(vec![0u8; 4]);
  let buf_b: Arc<[u8]> = Arc::from(vec![0u8; 4]);

  let err = IoOp::pin_vectored(
    &limiter,
    vec![(buf_a.clone(), 0..4), (buf_b.clone(), 0..2)],
  )
  .unwrap_err();
  assert!(matches!(err, IoLimitError::LimitExceeded("max pinned bytes")));
  assert_eq!(limiter.counters().pinned_bytes_current, 0);
  assert_eq!(limiter.counters().inflight_ops_current, 0);

  let op = IoOp::pin_vectored(&limiter, vec![(buf_a, 0..3), (buf_b, 0..2)]).unwrap();
  assert_eq!(limiter.counters().pinned_bytes_current, 5);
  assert_eq!(limiter.counters().inflight_ops_current, 1);
  drop(op);
  assert_eq!(limiter.counters().pinned_bytes_current, 0);
  assert_eq!(limiter.counters().inflight_ops_current, 0);
}
