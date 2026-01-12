use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use fastrender::image_compare::{compare_images, CompareConfig};
use image::{Rgba, RgbaImage};

mod common;

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

  let mut group = c.benchmark_group("compare_images");
  for case in &cases {
    let size_label = format!("{}x{}", case.width, case.height);
    let pixels = u64::from(case.width) * u64::from(case.height);
    group.throughput(Throughput::Elements(pixels));

    group.bench_function(BenchmarkId::new("identical", &size_label), |b| {
      b.iter(|| {
        let diff = compare_images(black_box(&case.base), black_box(&case.base), black_box(&config));
        black_box(diff.statistics.perceptual_distance);
      })
    });

    group.bench_function(BenchmarkId::new("few_pixels", &size_label), |b| {
      b.iter(|| {
        let diff =
          compare_images(black_box(&case.few_pixels), black_box(&case.base), black_box(&config));
        black_box(diff.statistics.perceptual_distance);
      })
    });

    group.bench_function(BenchmarkId::new("inverted", &size_label), |b| {
      b.iter(|| {
        let diff =
          compare_images(black_box(&case.inverted), black_box(&case.base), black_box(&config));
        black_box(diff.statistics.perceptual_distance);
      })
    });

    group.bench_function(BenchmarkId::new("dimension_mismatch", &size_label), |b| {
      b.iter(|| {
        let diff =
          compare_images(black_box(&case.mismatch), black_box(&case.base), black_box(&config));
        black_box(diff.dimensions_match);
      })
    });
  }

  group.finish();
}

criterion_group!(
  name = image_compare_benches;
  config = common::perf_criterion();
  targets = bench_compare_images
);
criterion_main!(image_compare_benches);
