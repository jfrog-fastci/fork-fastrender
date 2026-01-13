use crate::layout::float_context::{FloatContext, FloatSide};
use std::cmp::Ordering;

#[derive(Clone, Copy, Debug)]
struct TinyRng {
  state: u64,
}

impl TinyRng {
  fn new(seed: u64) -> Self {
    // Avoid the xorshift all-zero trap while keeping things deterministic.
    Self { state: seed | 1 }
  }

  fn next_u32(&mut self) -> u32 {
    // xorshift64: small, fast, deterministic, and good enough for tests.
    // Not cryptographically secure.
    self.state ^= self.state << 13;
    self.state ^= self.state >> 7;
    self.state ^= self.state << 17;
    (self.state >> 32) as u32
  }

  fn next_bool(&mut self) -> bool {
    (self.next_u32() & 1) != 0
  }

  fn next_f32_unit(&mut self) -> f32 {
    // [0, 1) with 24 bits of precision (avoids NaN/inf and keeps values finite).
    const SCALE: f32 = (1u32 << 24) as f32;
    ((self.next_u32() >> 8) as f32) / SCALE
  }

  fn f32_range(&mut self, min: f32, max: f32) -> f32 {
    debug_assert!(min.is_finite() && max.is_finite());
    min + (max - min) * self.next_f32_unit()
  }

  fn usize_range(&mut self, min: usize, max_inclusive: usize) -> usize {
    debug_assert!(min <= max_inclusive);
    let span = max_inclusive - min + 1;
    min + (self.next_u32() as usize % span)
  }

  fn i32_range(&mut self, min: i32, max_inclusive: i32) -> i32 {
    debug_assert!(min <= max_inclusive);
    let span = (max_inclusive - min + 1) as u32;
    min + (self.next_u32() % span) as i32
  }
}

#[derive(Clone, Copy, Debug)]
struct RectFloat {
  side: FloatSide,
  x: f32,
  y: f32,
  width: f32,
  height: f32,
}

impl RectFloat {
  fn top(self) -> f32 {
    self.y
  }

  fn bottom(self) -> f32 {
    self.y + self.height
  }

  fn left_edge(self) -> f32 {
    self.x
  }

  fn right_edge(self) -> f32 {
    self.x + self.width
  }

  fn active_at(self, y: f32) -> bool {
    y >= self.top() && y < self.bottom()
  }
}

fn clamp_positive_finite(value: f32) -> f32 {
  if value.is_finite() && value > 0.0 {
    value
  } else {
    0.0
  }
}

fn collect_sorted_unique_boundaries(floats: &[RectFloat]) -> Vec<f32> {
  let mut boundaries = Vec::with_capacity(floats.len().saturating_mul(2));
  for float in floats {
    boundaries.push(float.top());
    boundaries.push(float.bottom());
  }
  boundaries.sort_by(|a, b| a.total_cmp(b));
  boundaries.dedup_by(|a, b| *a == *b);
  boundaries
}

/// Reference implementation of rectangular-float "band edges" at a given y.
///
/// Active means `top <= y < bottom`.
fn naive_edges_at_y(
  floats: &[RectFloat],
  y: f32,
  containing_left: f32,
  containing_right: f32,
) -> (f32, f32) {
  let mut left_edge = containing_left;
  let mut right_edge = containing_right;

  for float in floats {
    if !float.active_at(y) {
      continue;
    }
    match float.side {
      FloatSide::Left => {
        left_edge = left_edge.max(float.right_edge());
      }
      FloatSide::Right => {
        right_edge = right_edge.min(float.left_edge());
      }
    }
  }

  (left_edge.max(containing_left), right_edge.min(containing_right))
}

/// Reference implementation of the minimum-width scan in `[start, end)` for rectangular floats.
///
/// Returns `(best_left, best_right, next_boundary)` where `next_boundary` is the next float start/end
/// boundary after the y at which the most constrained segment occurs.
fn naive_min_width_in_range(
  floats: &[RectFloat],
  boundaries: &[f32],
  start: f32,
  end: f32,
  containing_left: f32,
  containing_right: f32,
) -> (f32, f32, f32) {
  let start = if start.is_finite() { start } else { 0.0 };
  let end = if end.is_finite() { end.max(start) } else { start };
  let containing_left = if containing_left.is_finite() {
    containing_left
  } else {
    0.0
  };
  let containing_right = if containing_right.is_finite() {
    containing_right.max(containing_left)
  } else {
    containing_left
  };

  let (mut best_left, mut best_right) =
    naive_edges_at_y(floats, start, containing_left, containing_right);
  let mut best_width = (best_right - best_left).max(0.0);
  let mut best_y = start;

  let i0 = boundaries.partition_point(|&b| b.total_cmp(&start) == Ordering::Less);
  let i1 = boundaries.partition_point(|&b| b.total_cmp(&end) == Ordering::Less);

  for &y in boundaries[i0..i1].iter() {
    if y == start {
      continue;
    }
    let (left_edge, right_edge) = naive_edges_at_y(floats, y, containing_left, containing_right);
    let width = (right_edge - left_edge).max(0.0);

    if width < best_width - f32::EPSILON
      || ((width - best_width).abs() < f32::EPSILON && left_edge > best_left)
    {
      best_left = left_edge;
      best_right = right_edge;
      best_width = width;
      best_y = y;
    }
  }

  let next_idx = boundaries.partition_point(|&b| b.total_cmp(&best_y) != Ordering::Greater);
  let next_boundary = boundaries
    .get(next_idx)
    .copied()
    .unwrap_or(f32::INFINITY);

  (best_left, best_right, next_boundary)
}

/// Reference implementation of `next_float_boundary_after` for rectangular floats.
///
/// This matches the optimized behavior in `FloatContext`: the next boundary is the minimum of:
/// - the next float *start* y strictly greater than `y`
/// - the bottom of the currently-constraining left float (max right-edge; tie max bottom)
/// - the bottom of the currently-constraining right float (min left-edge; tie max bottom)
fn naive_next_boundary_after(floats: &[RectFloat], y: f32) -> f32 {
  let mut next_start = f32::INFINITY;

  let mut left_best_edge = f32::NEG_INFINITY;
  let mut left_best_bottom = f32::INFINITY;
  let mut left_found = false;

  let mut right_best_edge = f32::INFINITY;
  let mut right_best_bottom = f32::INFINITY;
  let mut right_found = false;

  for float in floats {
    let top = float.top();
    if top > y && top < next_start {
      next_start = top;
    }

    if !float.active_at(y) {
      continue;
    }

    match float.side {
      FloatSide::Left => {
        let edge = float.right_edge();
        let bottom = float.bottom();
        if !left_found || edge > left_best_edge || (edge == left_best_edge && bottom > left_best_bottom) {
          left_found = true;
          left_best_edge = edge;
          left_best_bottom = bottom;
        }
      }
      FloatSide::Right => {
        let edge = float.left_edge();
        let bottom = float.bottom();
        if !right_found
          || edge < right_best_edge
          || (edge == right_best_edge && bottom > right_best_bottom)
        {
          right_found = true;
          right_best_edge = edge;
          right_best_bottom = bottom;
        }
      }
    }
  }

  let mut next = next_start;
  if left_found {
    next = next.min(left_best_bottom);
  }
  if right_found {
    next = next.min(right_best_bottom);
  }

  next
}

fn choose_query_y(rng: &mut TinyRng, boundaries: &[f32], min: f32, max: f32) -> f32 {
  if !boundaries.is_empty() && (rng.next_u32() % 4 == 0) {
    boundaries[rng.usize_range(0, boundaries.len() - 1)]
  } else {
    rng.f32_range(min, max)
  }
}

#[test]
fn float_context_rectangular_naive_equivalence() {
  const SEED_COUNT: u64 = 50;
  const WIDTH_QUERIES: usize = 200;
  // Range scans are the most expensive part of the equivalence checks (O(floats * boundaries) per
  // query). Keep the count moderate so the test stays fast in debug builds.
  const RANGE_QUERIES: usize = 40;
  const BOUNDARY_QUERIES: usize = 200;
  const OFFSET_WIDTH_QUERIES: usize = 100;
  const OFFSET_RANGE_QUERIES: usize = 20;

  for seed in 0..SEED_COUNT {
    // Mix the seed a bit so neighboring seeds don't produce nearly-identical float fields.
    let mut rng = TinyRng::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1));

    let container_width = rng.f32_range(50.0, 400.0);
    let mut ctx = FloatContext::new(container_width);

    let float_count = rng.usize_range(50, 200);
    let mut floats = Vec::with_capacity(float_count);

    for _ in 0..float_count {
      let side = if rng.next_bool() {
        FloatSide::Left
      } else {
        FloatSide::Right
      };

      // Quantize float geometry so we frequently hit identical edges/boundaries (important for
      // exercising ActiveEdgeSet coalescing and dense start-event scenarios).
      let y = (rng.i32_range(-12, 64) as f32) * 5.0; // [-60, 320] step 5
      let height = (rng.i32_range(1, 16) as f32) * 5.0; // [5, 80] step 5

      // Allow coordinates slightly outside the containing block to exercise offset-context logic.
      let x_step = container_width / 16.0;
      let x = (rng.i32_range(-8, 24) as f32) * x_step; // [-0.5cw, 1.5cw] step cw/16
      let width = (rng.i32_range(1, 24) as f32) * x_step; // [cw/16, 1.5cw] step cw/16

      ctx.add_float_at(side, x, y, width, height);
      floats.push(RectFloat {
        side,
        x,
        y,
        width,
        height,
      });
    }

    let boundaries = collect_sorted_unique_boundaries(&floats);

    // 1) available_width_at_y matches a naive scan.
    for i in 0..WIDTH_QUERIES {
      let y = choose_query_y(&mut rng, &boundaries, -80.0, 420.0);
      let (ctx_left, ctx_width) = ctx.available_width_at_y(y);
      let (naive_left, naive_right) = naive_edges_at_y(&floats, y, 0.0, container_width);
      let naive_width = (naive_right - naive_left).max(0.0);

      assert_eq!(
        (ctx_left, ctx_width),
        (naive_left, naive_width),
        "seed={seed} width_query={i} y={y}"
      );
    }

    // 2) available_width_at_y_in_containing_block matches the naive scan with span clamping.
    for i in 0..OFFSET_WIDTH_QUERIES {
      let y = choose_query_y(&mut rng, &boundaries, -80.0, 420.0);
      let containing_left = rng.f32_range(-container_width, container_width);
      // Occasionally generate non-positive widths so we exercise `clamp_positive_finite`.
      let containing_width_raw = if (rng.next_u32() % 8) == 0 {
        -rng.f32_range(0.0, container_width)
      } else {
        rng.f32_range(0.0, container_width * 2.0)
      };
      let (ctx_left, ctx_width) = ctx.available_width_at_y_in_containing_block(
        y,
        containing_left,
        containing_width_raw,
      );

      let containing_width = clamp_positive_finite(containing_width_raw);
      let containing_right = containing_left + containing_width;
      let (naive_left, naive_right) =
        naive_edges_at_y(&floats, y, containing_left, containing_right);
      let naive_width = (naive_right - naive_left).max(0.0);

      assert_eq!(
        (ctx_left, ctx_width),
        (naive_left, naive_width),
        "seed={seed} offset_width_query={i} y={y} span=({containing_left},{containing_width_raw})"
      );
    }

    // 3) available_width_in_range matches a naive min-width scan (including tie-breaker).
    for i in 0..RANGE_QUERIES {
      let start = choose_query_y(&mut rng, &boundaries, -80.0, 420.0);
      let end = choose_query_y(&mut rng, &boundaries, -80.0, 420.0);
      let (ctx_left, ctx_width) = ctx.available_width_in_range(start, end);

      let end = end.max(start);
      let (naive_left, naive_right, _next_boundary) =
        naive_min_width_in_range(&floats, &boundaries, start, end, 0.0, container_width);
      let naive_width = (naive_right - naive_left).max(0.0);

      assert_eq!(
        (ctx_left, ctx_width),
        (naive_left, naive_width),
        "seed={seed} range_query={i} start={start} end={end}"
      );
    }

    // 4) available_width_in_range_in_containing_block matches the naive scan with span clamping.
    for i in 0..OFFSET_RANGE_QUERIES {
      let start = choose_query_y(&mut rng, &boundaries, -80.0, 420.0);
      let end = choose_query_y(&mut rng, &boundaries, -80.0, 420.0);

      let containing_left = rng.f32_range(-container_width, container_width);
      let containing_width_raw = if (rng.next_u32() % 8) == 0 {
        -rng.f32_range(0.0, container_width)
      } else {
        rng.f32_range(0.0, container_width * 2.0)
      };

      let (ctx_left, ctx_width) = ctx.available_width_in_range_in_containing_block(
        start,
        end,
        containing_left,
        containing_width_raw,
      );

      let end = end.max(start);
      let containing_width = clamp_positive_finite(containing_width_raw);
      let containing_right = containing_left + containing_width;
      let (naive_left, naive_right, _next_boundary) = naive_min_width_in_range(
        &floats,
        &boundaries,
        start,
        end,
        containing_left,
        containing_right,
      );
      let naive_width = (naive_right - naive_left).max(0.0);

      assert_eq!(
        (ctx_left, ctx_width),
        (naive_left, naive_width),
        "seed={seed} offset_range_query={i} start={start} end={end} span=({containing_left},{containing_width_raw})"
      );
    }

    // 5) next_float_boundary_after matches a naive boundary computation for rectangular floats.
    for i in 0..BOUNDARY_QUERIES {
      let y = choose_query_y(&mut rng, &boundaries, -80.0, 420.0);
      let ctx_next = ctx.next_float_boundary_after(y);
      let naive_raw = naive_next_boundary_after(&floats, y);
      let naive_next = if naive_raw.is_finite() && naive_raw > y {
        naive_raw
      } else {
        y
      };

      assert_eq!(
        ctx_next, naive_next,
        "seed={seed} boundary_query={i} y={y} naive_raw={naive_raw}"
      );
    }
  }
}
