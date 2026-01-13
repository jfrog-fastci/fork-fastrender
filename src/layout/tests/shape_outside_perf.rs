use crate::error::{RenderError, RenderStage};
use crate::layout::float_context::{FloatContext, FloatSide};
use crate::layout::float_shape::FloatShape;
use crate::layout::formatting_context::LayoutError;
use crate::render_control::{check_active_periodic, with_deadline, RenderDeadline};
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

fn naive_next_change_after(shape: &FloatShape, y: f32) -> Option<f32> {
  let len = (shape.bottom() - shape.top()).max(0.0) as usize;
  if len == 0 {
    return None;
  }
  let mut idx = ((y - shape.top()).floor() as isize + 1).max(0) as usize;
  let current = shape.span_at(y);
  while idx < len {
    let row_y = shape.top() + idx as f32;
    if shape.span_at(row_y) != current {
      return Some(row_y);
    }
    idx += 1;
  }
  None
}

#[test]
fn float_shape_next_change_after_matches_naive_scan() {
  let patterns: Vec<(f32, Vec<Option<(f32, f32)>>)> = vec![
    (
      10.0,
      vec![
        None,
        None,
        Some((0.0, 10.0)),
        Some((0.0, 10.0)),
        None,
        Some((5.0, 15.0)),
        Some((5.0, 15.0)),
        None,
      ],
    ),
    (0.0, vec![Some((0.0, 10.0)); 16]),
    (3.5, vec![None; 16]),
    (
      -2.0,
      vec![
        None,
        Some((0.0, 1.0)),
        None,
        Some((0.0, 1.0)),
        None,
        Some((10.0, 20.0)),
        Some((10.0, 20.0)),
        Some((10.0, 20.0)),
        None,
        None,
        Some((0.0, 1.0)),
      ],
    ),
  ];

  for (start_y, spans) in patterns {
    let shape = FloatShape::from_spans_for_test(start_y, spans);
    let bottom = shape.bottom();
    let mut y = start_y - 3.0;
    while y <= bottom + 3.0 {
      let expected = naive_next_change_after(&shape, y);
      let actual = shape.next_change_after(y);
      assert_eq!(
        actual, expected,
        "next_change_after mismatch (start_y={start_y}, y={y})"
      );
      y += 0.25;
    }
  }
}

#[test]
fn shape_outside_boundary_queries_complete_before_deadline() {
  // Ensure deadline checks are not artificially slowed down by other tests or environment
  // variables (e.g. `FASTR_TEST_RENDER_DELAY_MS`).
  let _delay_guard = TestRenderDelayGuard::set(Some(0));

  // Tight enough to catch regressions quickly, with slack for slower CI.
  let deadline = RenderDeadline::new(Some(Duration::from_millis(1500)), None);

  const ROWS: usize = 50_000;
  let spans = vec![None; ROWS];
  let shape = FloatShape::from_spans_for_test(0.0, spans);

  let mut ctx = FloatContext::new(100.0);
  ctx.add_float_with_shape(
    FloatSide::Left,
    0.0,
    0.0,
    10.0,
    ROWS as f32,
    Some(shape),
  );

  let (result, timeout, acc) = with_deadline(Some(&deadline), || {
    let mut counter = 0usize;
    let mut acc = 0.0f32;
    for i in 0..ROWS {
      let y = i as f32;
      let (left, width) = ctx.available_width_at_y(y);
      let boundary = ctx.next_float_boundary_after(y);
      acc += left + width + boundary;

      if let Err(err) = check_active_periodic(&mut counter, 256, RenderStage::Layout) {
        return (Err(err), ctx.take_timeout_error(), acc);
      }
    }
    (Ok(()), ctx.take_timeout_error(), acc)
  });

  std::hint::black_box(acc);

  match result {
    Ok(()) => {}
    Err(RenderError::Timeout { elapsed, .. }) => panic!(
      "expected shape-outside boundary queries to finish under deadline, timed out after {elapsed:?}"
    ),
    Err(other) => panic!("unexpected render error: {other:?}"),
  }

  match timeout {
    None => {}
    Some(LayoutError::Timeout { elapsed }) => panic!(
      "expected shape-outside boundary queries to finish under deadline, timed out after {elapsed:?}"
    ),
    Some(other) => panic!("unexpected layout error: {other:?}"),
  }
}

