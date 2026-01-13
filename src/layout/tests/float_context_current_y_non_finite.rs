use crate::layout::float_context::{FloatContext, FloatSide};
use crate::layout::formatting_context::LayoutError;
use crate::render_control::{with_deadline, RenderDeadline};
use std::time::Duration;

struct TestRenderDelayGuard;

impl TestRenderDelayGuard {
  fn set(ms: Option<u64>) -> Self {
    crate::render_control::set_test_render_delay_ms(ms);
    Self
  }
}

impl Drop for TestRenderDelayGuard {
  fn drop(&mut self) {
    crate::render_control::set_test_render_delay_ms(None);
  }
}

#[test]
fn float_context_set_current_y_ignores_non_finite() {
  let mut ctx = FloatContext::new(800.0);
  ctx.set_current_y(10.0);
  assert_eq!(ctx.current_y(), 10.0);

  ctx.set_current_y(f32::NAN);
  assert!(ctx.current_y().is_finite());
  assert_eq!(ctx.current_y(), 10.0);

  ctx.set_current_y(f32::INFINITY);
  assert!(ctx.current_y().is_finite());
  assert_eq!(ctx.current_y(), 10.0);

  ctx.set_current_y(f32::NEG_INFINITY);
  assert!(ctx.current_y().is_finite());
  assert_eq!(ctx.current_y(), 10.0);
}

#[test]
fn float_context_advance_y_ignores_non_finite_and_overflow() {
  let mut ctx = FloatContext::new(800.0);
  ctx.set_current_y(1.0);

  ctx.advance_y(f32::NAN);
  assert!(ctx.current_y().is_finite());
  assert_eq!(ctx.current_y(), 1.0);

  ctx.advance_y(f32::INFINITY);
  assert!(ctx.current_y().is_finite());
  assert_eq!(ctx.current_y(), 1.0);

  ctx.set_current_y(f32::MAX);
  ctx.advance_y(f32::MAX); // Would overflow to +inf without guarding.
  assert!(ctx.current_y().is_finite());
  assert_eq!(ctx.current_y(), f32::MAX);
}

#[test]
fn float_context_current_y_non_finite_does_not_timeout() {
  let _delay_guard = TestRenderDelayGuard::set(Some(0));

  // Keep a tight-ish budget so NaN/inf poisoning (which can cause pathological sweep behavior)
  // gets caught quickly, while still avoiding flakiness on slower CI.
  let deadline = RenderDeadline::new(Some(Duration::from_millis(1500)), None);

  const CONTAINING_WIDTH: f32 = 200.0;
  const FLOAT_WIDTH: f32 = 80.0;
  const FLOAT_HEIGHT: f32 = 1.0;
  const FLOAT_COUNT: usize = 10_000;

  let (acc, timeout) = with_deadline(Some(&deadline), || {
    let mut ctx = FloatContext::new(CONTAINING_WIDTH);

    // Try to poison `current_y` with non-finite values. The public API should ignore these
    // mutations so subsequent placement uses a finite, monotonic y.
    ctx.set_current_y(f32::NAN);
    ctx.set_current_y(f32::INFINITY);
    ctx.set_current_y(f32::NEG_INFINITY);
    ctx.advance_y(f32::NAN);
    ctx.advance_y(f32::INFINITY);

    let mut acc = 0.0f32;
    for i in 0..FLOAT_COUNT {
      let side = if i % 2 == 0 {
        FloatSide::Left
      } else {
        FloatSide::Right
      };
      // Typical callsite pattern: pass the float context's current y as the minimum y for float
      // placement.
      let min_y = ctx.current_y();
      let (x, y) = ctx.compute_float_position(side, FLOAT_WIDTH, FLOAT_HEIGHT, min_y);
      assert!(x.is_finite() && y.is_finite());
      ctx.add_float_at(side, x, y, FLOAT_WIDTH, FLOAT_HEIGHT);

      // Interleave width queries similar to inline layout.
      if i % 64 == 0 {
        let (left, width) = ctx.available_width_at_y(y);
        acc += left + width;
      }
    }

    let timeout = ctx.take_timeout_error();
    (acc, timeout)
  });

  std::hint::black_box(acc);
  match timeout {
    None => {}
    Some(LayoutError::Timeout { elapsed }) => panic!(
      "expected float placement/queries to complete under deadline after non-finite current_y inputs, timed out after {elapsed:?}"
    ),
    Some(other) => panic!("unexpected layout error: {other:?}"),
  }
}

