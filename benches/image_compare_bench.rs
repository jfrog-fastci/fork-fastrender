use criterion::{
  black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput,
};
use fastrender::image_compare::{compare_images, CompareConfig};
use image::{Rgba, RgbaImage};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

mod common;

// -----------------------------------------------------------------------------
// Allocation tracking
// -----------------------------------------------------------------------------

struct CountingAllocator;

static ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);
static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
    System.alloc(layout)
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
    System.alloc_zeroed(layout)
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    ALLOC_BYTES.fetch_add(new_size, Ordering::Relaxed);
    System.realloc(ptr, layout, new_size)
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    System.dealloc(ptr, layout)
  }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn allocation_counts() -> (usize, usize) {
  (
    ALLOC_CALLS.load(Ordering::Relaxed),
    ALLOC_BYTES.load(Ordering::Relaxed),
  )
}

/// Keep sizes modest but non-trivial so this bench catches accidental O(N^2) scaling in the
/// perceptual distance implementation (e.g. if a downsampled/windowed SSIM path regresses).
const SIZES: &[(u32, u32)] = &[(512, 512), (1024, 768)];

fn patterned_image(width: u32, height: u32) -> RgbaImage {
  // Deterministic structured pattern with non-trivial per-channel variation so SSIM has
  // meaningful variance (avoid degenerate all-solid images).
  let mut buf = vec![0u8; (width as usize) * (height as usize) * 4];

  for y in 0..height {
    for x in 0..width {
      let idx = ((y * width + x) * 4) as usize;
      // Cheap hash of (x,y) into 0..255.
      let r = (x.wrapping_mul(31).wrapping_add(y.wrapping_mul(17)) & 0xFF) as u8;
      let g = (x.wrapping_mul(13).wrapping_add(y.wrapping_mul(29)) & 0xFF) as u8;
      let b = (x.wrapping_mul(7).wrapping_add(y.wrapping_mul(3)) & 0xFF) as u8;
      buf[idx] = r;
      buf[idx + 1] = g;
      buf[idx + 2] = b;
      buf[idx + 3] = 255;
    }
  }

  RgbaImage::from_raw(width, height, buf).expect("valid pattern image")
}

fn flip_some_pixels(img: &mut RgbaImage, count: u32) {
  // Flip a small number of pixels in a deterministic spread so we approximate a "typical"
  // near-identical render diff (e.g. text raster jitter).
  //
  // Uses a tiny LCG to generate positions without depending on RNG crates.
  let mut state = 0x1234_5678u32;
  let width = img.width();
  let height = img.height();

  for _ in 0..count {
    // LCG constants from Numerical Recipes.
    state = state.wrapping_mul(1664525).wrapping_add(1013904223);
    let x = state % width;
    state = state.wrapping_mul(1664525).wrapping_add(1013904223);
    let y = state % height;

    let px = img.get_pixel_mut(x, y);
    // Small but non-zero delta so strict compare flags it.
    px.0[0] = px.0[0].wrapping_add(1);
  }
}

fn invert_image(img: &RgbaImage) -> RgbaImage {
  let mut out = img.clone();
  for px in out.pixels_mut() {
    let [r, g, b, a] = px.0;
    *px = Rgba([255u8.wrapping_sub(r), 255u8.wrapping_sub(g), 255u8.wrapping_sub(b), a]);
  }
  out
}

struct ImageCases {
  width: u32,
  height: u32,
  base: RgbaImage,
  few_pixels: RgbaImage,
  inverted: RgbaImage,
  mismatch: RgbaImage,
}

fn bench_compare_images(c: &mut Criterion) {
  common::bench_print_config_once("image_compare_bench", &[]);

  let cases: Vec<ImageCases> = SIZES
    .iter()
    .copied()
    .map(|(width, height)| {
      let base = patterned_image(width, height);
      let mut few_pixels = base.clone();
      flip_some_pixels(&mut few_pixels, 128);
      let inverted = invert_image(&base);
      let mismatch = patterned_image(width + 1, height);

      ImageCases {
        width,
        height,
        base,
        few_pixels,
        inverted,
        mismatch,
      }
    })
    .collect();

  // Measure metric computation only; avoid diff image generation / PNG encoding.
  let config = CompareConfig::strict().with_generate_diff_image(false);
  let config = &config;

  let mut group = c.benchmark_group("compare_images");
  for case in &cases {
    let width = case.width;
    let height = case.height;
    let pixels = u64::from(width) * u64::from(height);
    group.throughput(Throughput::Elements(pixels));

    let base = &case.base;
    let few_pixels = &case.few_pixels;
    let inverted = &case.inverted;
    let mismatch = &case.mismatch;

    let printed_identical = AtomicBool::new(false);
    group.bench_function(BenchmarkId::new("identical", format!("{width}x{height}")), move |b| {
      b.iter_custom(|iters| {
        let (calls_start, bytes_start) = allocation_counts();
        let start = Instant::now();
        for _ in 0..iters {
          let diff = compare_images(black_box(base), black_box(base), black_box(config));
          black_box(diff.statistics.perceptual_distance);
        }
        let duration = start.elapsed();
        let (calls_end, bytes_end) = allocation_counts();
        let calls = calls_end.saturating_sub(calls_start);
        let bytes = bytes_end.saturating_sub(bytes_start);
        black_box((calls, bytes));

        if !printed_identical.swap(true, Ordering::Relaxed) {
          let iters_usize = iters as usize;
          let per_call_calls = calls / iters_usize.max(1);
          let per_call_bytes = bytes / iters_usize.max(1);
          eprintln!(
            "compare_images/identical/{width}x{height} allocations/call: calls={per_call_calls} bytes={per_call_bytes}"
          );
        }

        duration
      });
    });

    let printed_few_pixels = AtomicBool::new(false);
    group.bench_function(BenchmarkId::new("few_pixels", format!("{width}x{height}")), move |b| {
      b.iter_custom(|iters| {
        let (calls_start, bytes_start) = allocation_counts();
        let start = Instant::now();
        for _ in 0..iters {
          let diff = compare_images(
            black_box(few_pixels),
            black_box(base),
            black_box(config),
          );
          black_box(diff.statistics.perceptual_distance);
        }
        let duration = start.elapsed();
        let (calls_end, bytes_end) = allocation_counts();
        let calls = calls_end.saturating_sub(calls_start);
        let bytes = bytes_end.saturating_sub(bytes_start);
        black_box((calls, bytes));

        if !printed_few_pixels.swap(true, Ordering::Relaxed) {
          let iters_usize = iters as usize;
          let per_call_calls = calls / iters_usize.max(1);
          let per_call_bytes = bytes / iters_usize.max(1);
          eprintln!(
            "compare_images/few_pixels/{width}x{height} allocations/call: calls={per_call_calls} bytes={per_call_bytes}"
          );
        }

        duration
      });
    });

    let printed_inverted = AtomicBool::new(false);
    group.bench_function(BenchmarkId::new("inverted", format!("{width}x{height}")), move |b| {
      b.iter_custom(|iters| {
        let (calls_start, bytes_start) = allocation_counts();
        let start = Instant::now();
        for _ in 0..iters {
          let diff = compare_images(
            black_box(inverted),
            black_box(base),
            black_box(config),
          );
          black_box(diff.statistics.perceptual_distance);
        }
        let duration = start.elapsed();
        let (calls_end, bytes_end) = allocation_counts();
        let calls = calls_end.saturating_sub(calls_start);
        let bytes = bytes_end.saturating_sub(bytes_start);
        black_box((calls, bytes));

        if !printed_inverted.swap(true, Ordering::Relaxed) {
          let iters_usize = iters as usize;
          let per_call_calls = calls / iters_usize.max(1);
          let per_call_bytes = bytes / iters_usize.max(1);
          eprintln!(
            "compare_images/inverted/{width}x{height} allocations/call: calls={per_call_calls} bytes={per_call_bytes}"
          );
        }

        duration
      });
    });

    let printed_mismatch = AtomicBool::new(false);
    group.bench_function(
      BenchmarkId::new("dimension_mismatch", format!("{width}x{height}")),
      move |b| {
      b.iter_custom(|iters| {
        let (calls_start, bytes_start) = allocation_counts();
        let start = Instant::now();
        for _ in 0..iters {
          let diff = compare_images(
            black_box(mismatch),
            black_box(base),
            black_box(config),
          );
          black_box(diff.dimensions_match);
        }
        let duration = start.elapsed();
        let (calls_end, bytes_end) = allocation_counts();
        let calls = calls_end.saturating_sub(calls_start);
        let bytes = bytes_end.saturating_sub(bytes_start);
        black_box((calls, bytes));

        if !printed_mismatch.swap(true, Ordering::Relaxed) {
          let iters_usize = iters as usize;
          let per_call_calls = calls / iters_usize.max(1);
          let per_call_bytes = bytes / iters_usize.max(1);
          eprintln!(
            "compare_images/dimension_mismatch/{width}x{height} allocations/call: calls={per_call_calls} bytes={per_call_bytes}"
          );
        }

        duration
      });
    },
    );
  }

  group.finish();
}

criterion_group!(
  name = image_compare_benches;
  config = common::perf_criterion();
  targets = bench_compare_images
);
criterion_main!(image_compare_benches);
