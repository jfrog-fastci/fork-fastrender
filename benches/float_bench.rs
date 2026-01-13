use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use fastrender::layout::float_context::{FloatContext, FloatSide};
use fastrender::layout::inline::float_integration::{InlineFloatIntegration, LineSpaceOptions};

mod common;

fn build_float_context(count: usize) -> FloatContext {
  let mut ctx = FloatContext::new(200.0);
  for i in 0..count {
    let y = i as f32;
    if i % 2 == 0 {
      ctx.add_float_at(FloatSide::Left, 0.0, y, 80.0, 1.0);
    } else {
      ctx.add_float_at(FloatSide::Right, 120.0, y, 80.0, 1.0);
    }
  }
  ctx
}

fn bench_available_width(c: &mut Criterion) {
  common::bench_print_config_once("float_bench", &[]);
  let ctx = build_float_context(5_000);
  c.bench_function("float_available_width_dense", |b| {
    b.iter(|| {
      let mut y = 0.0f32;
      while y < 5_000.0 {
        black_box(ctx.available_width_at_y(y));
        y += 0.5;
      }
    })
  });
}

fn bench_compute_float_position(c: &mut Criterion) {
  common::bench_print_config_once("float_bench", &[]);
  c.bench_function("float_place_many", |b| {
    b.iter(|| {
      let mut ctx = build_float_context(2_500);
      let mut y = 0.0f32;
      for _ in 0..200 {
        let (x, placed_y) = ctx.compute_float_position(FloatSide::Left, 40.0, 2.0, y);
        ctx.add_float_at(FloatSide::Left, x, placed_y, 40.0, 2.0);
        y = placed_y;
      }
    })
  });
}

fn bench_available_width_in_range(c: &mut Criterion) {
  common::bench_print_config_once("float_bench", &[]);
  let ctx = build_float_context(5_000);
  c.bench_function("float_available_width_in_range_dense", |b| {
    b.iter(|| {
      let mut y = 0.0f32;
      while y < 5_000.0 {
        black_box(ctx.available_width_in_range(y, y + 20.0));
        y += 0.5;
      }
    })
  });
}

fn bench_inline_find_line_space(c: &mut Criterion) {
  common::bench_print_config_once("float_bench", &[]);
  let ctx = build_float_context(5_000);
  let integration = InlineFloatIntegration::new(&ctx);
  // Use a positive `line_height` so inline layout triggers the float fit search. With the dense
  // alternating float pattern above, the requested width always fits immediately, so each call
  // should scan float ranges exactly once (previously the call site would re-query the same range
  // to recover the edges).
  let opts = LineSpaceOptions::with_min_width(100.0).line_height(20.0);
  c.bench_function("float_inline_find_line_space_dense", |b| {
    b.iter(|| {
      let mut y = 0.0f32;
      while y < 5_000.0 {
        black_box(integration.find_line_space(y, opts));
        y += 0.5;
      }
    })
  });
}

fn bench_find_fit_dense_boundaries(c: &mut Criterion) {
  common::bench_print_config_once("float_bench", &[]);
  // Dense-boundary stress: alternating left/right floats every 1px for many rows, then ask `find_fit`
  // for a tall box that cannot fit until below all floats. This historically triggered repeated
  // rescans of overlapping FloatRangeCache segments.
  let ctx = build_float_context(10_000);
  c.bench_function("float_find_fit_dense_boundaries", |b| {
    b.iter(|| {
      // Keep the query height large enough to span many boundaries.
      black_box(ctx.find_fit(150.0, 500.0, 0.0));
    })
  });
}

fn bench_compute_float_position_overlap_stress(c: &mut Criterion) {
  common::bench_print_config_once("float_bench", &[]);
  // A stress pattern that resembles float-heavy real sites:
  // - many overlapping floats (varying heights) starting at the same y
  // - small float widths so each "row" packs many floats
  // - repeated `compute_float_position` calls that often must step through many float-boundary
  //   events to find a y that fits
  c.bench_function("float_place_overlap_boundary_steps", |b| {
    b.iter(|| {
      let mut ctx = FloatContext::new(320.0);
      let float_width = 5.0f32;
      let floats_per_row = (ctx.containing_block_width() / float_width) as usize;
      let total_floats = 2_000usize;

      for i in 0..total_floats {
        // Heights increase left-to-right within a packed row so when the row is full the next float
        // must often step through many distinct end boundaries before the constraining rightmost
        // float ends.
        let height = 1.0 + (i % floats_per_row) as f32;
        let (x, y) = ctx.compute_float_position(FloatSide::Left, float_width, height, 0.0);
        ctx.add_float_at(FloatSide::Left, x, y, float_width, height);
      }

      black_box(ctx.float_count());
    })
  });
}

fn bench_range_cache_incremental_updates(c: &mut Criterion) {
  common::bench_print_config_once("float_bench", &[]);
  // Stress `FloatRangeCache::apply_rect_float` by:
  // 1) Building a large cached segment list via a wide range query.
  // 2) Inserting many floats that each cause many cached segments to merge.
  c.bench_function("float_range_cache_incremental_updates", |b| {
    b.iter_batched(
      || {
        const BASE_SEGMENTS: usize = 20_000;

        let mut ctx = FloatContext::new(10_000.0);
        for i in 0..BASE_SEGMENTS {
          let width = (BASE_SEGMENTS - i) as f32;
          let height = (i + 1) as f32;
          ctx.add_float_at(FloatSide::Left, 0.0, 0.0, width, height);
        }

        // Populate the range cache with many distinct segments.
        black_box(ctx.available_width_in_range(0.0, BASE_SEGMENTS as f32));
        ctx
      },
      |mut ctx| {
        const BASE_SEGMENTS: usize = 20_000;
        const WINDOW: usize = 100;
        const UPDATE_COUNT: usize = 180;

        // Trigger incremental cache updates that perform heavy coalescing.
        for i in 0..UPDATE_COUNT {
          let y = (i * WINDOW) as f32;
          let width = (BASE_SEGMENTS - i * WINDOW) as f32;
          ctx.add_float_at(FloatSide::Left, 0.0, y, width, WINDOW as f32);
        }

        black_box(ctx.float_count());
      },
      BatchSize::LargeInput,
    )
  });
}

fn build_shared_edge_late_prune_context() -> FloatContext {
  const CONTAINING_WIDTH: f32 = 200.0;
  const CONSTRAINING_FLOAT_WIDTH: f32 = 80.0;
  const CONSTRAINING_FLOAT_HEIGHT: f32 = 10_000.0;
  const NON_CONSTRAINING_FLOAT_WIDTH: f32 = 10.0;
  const NON_CONSTRAINING_FLOAT_COUNT: usize = 10_000;

  let mut ctx = FloatContext::new(CONTAINING_WIDTH);
  ctx.add_float_at(
    FloatSide::Left,
    0.0,
    0.0,
    CONSTRAINING_FLOAT_WIDTH,
    CONSTRAINING_FLOAT_HEIGHT,
  );
  for i in 0..NON_CONSTRAINING_FLOAT_COUNT {
    ctx.add_float_at(
      FloatSide::Left,
      0.0,
      0.0,
      NON_CONSTRAINING_FLOAT_WIDTH,
      (i + 1) as f32,
    );
  }
  ctx
}

fn bench_shared_edge_late_prune(c: &mut Criterion) {
  common::bench_print_config_once("float_bench", &[]);
  c.bench_function("float_shared_edge_late_prune", |b| {
    b.iter_batched(
      build_shared_edge_late_prune_context,
      |ctx| black_box(ctx.available_width_at_y(10_000.0)),
      BatchSize::LargeInput,
    )
  });
}

criterion_group!(
  float_benches,
  bench_available_width,
  bench_available_width_in_range,
  bench_inline_find_line_space,
  bench_find_fit_dense_boundaries,
  bench_compute_float_position,
  bench_compute_float_position_overlap_stress,
  bench_range_cache_incremental_updates,
  bench_shared_edge_late_prune
);
criterion_main!(float_benches);
