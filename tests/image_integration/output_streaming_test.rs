//! Streaming/row-based image encoding regressions.
//!
//! These tests focus on `image_output::encode_image`, ensuring:
//! - premultiplied tiny-skia pixmaps are correctly unpremultiplied before encoding
//! - encoders do not allocate full-frame temporary buffers for PNG output

use fastrender::image_output::{encode_image, OutputFormat};
use fastrender::Pixmap;
use image::GenericImageView;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

struct MaxAllocRecorder;

static MAX_ALLOC: AtomicUsize = AtomicUsize::new(0);

// These tests use a global allocator to track peak allocation size. The Rust test harness runs
// tests in parallel by default, so guard each test with a mutex to keep allocations deterministic
// and prevent racy `MAX_ALLOC` resets.
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock_tests() -> MutexGuard<'static, ()> {
  TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner())
}

#[global_allocator]
static GLOBAL_ALLOC: MaxAllocRecorder = MaxAllocRecorder;

unsafe impl GlobalAlloc for MaxAllocRecorder {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    record_alloc(layout.size());
    System.alloc(layout)
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    record_alloc(layout.size());
    System.alloc_zeroed(layout)
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    System.dealloc(ptr, layout)
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    record_alloc(new_size);
    System.realloc(ptr, layout, new_size)
  }
}

fn record_alloc(size: usize) {
  let mut current = MAX_ALLOC.load(Ordering::Relaxed);
  while size > current {
    match MAX_ALLOC.compare_exchange(current, size, Ordering::Relaxed, Ordering::Relaxed) {
      Ok(_) => break,
      Err(actual) => current = actual,
    }
  }
}

fn reset_max_alloc() {
  MAX_ALLOC.store(0, Ordering::Relaxed);
}

fn max_alloc() -> usize {
  MAX_ALLOC.load(Ordering::Relaxed)
}

fn unpremultiply_rgb(r: u8, g: u8, b: u8, a: u8) -> (u8, u8, u8) {
  if a == 0 {
    return (0, 0, 0);
  }
  let alpha = a as f32 / 255.0;
  (
    ((r as f32 / alpha).min(255.0)) as u8,
    ((g as f32 / alpha).min(255.0)) as u8,
    ((b as f32 / alpha).min(255.0)) as u8,
  )
}

fn decode_rgba(bytes: &[u8]) -> image::RgbaImage {
  image::load_from_memory(bytes)
    .expect("encoded bytes should decode")
    .to_rgba8()
}

#[test]
fn png_round_trip_1x1_unpremultiplies_exactly() {
  let _guard = lock_tests();
  let mut pixmap = Pixmap::new(1, 1).expect("pixmap");
  // Premultiplied: r = a = 10 => unpremultiply should clamp to 255.
  pixmap.data_mut().copy_from_slice(&[10, 0, 0, 10]);

  let encoded = encode_image(&pixmap, OutputFormat::Png).expect("png encode");
  let decoded = decode_rgba(&encoded);
  let (r, g, b) = unpremultiply_rgb(10, 0, 0, 10);
  assert_eq!(decoded.get_pixel(0, 0).0, [r, g, b, 10]);
}

#[test]
fn png_round_trip_2x2_unpremultiplies_exactly() {
  let _guard = lock_tests();
  let mut pixmap = Pixmap::new(2, 2).expect("pixmap");
  // Row-major premultiplied RGBA pixels.
  pixmap.data_mut().copy_from_slice(&[
    10, 0, 0, 10, // (0,0) -> (255,0,0,10)
    0, 20, 0, 40, // (1,0) -> (0,127,0,40)
    0, 0, 30, 60, // (0,1) -> (0,0,127,60)
    40, 40, 40, 40, // (1,1) -> (255,255,255,40)
  ]);

  let encoded = encode_image(&pixmap, OutputFormat::Png).expect("png encode");
  let decoded = decode_rgba(&encoded);

  let (r0, g0, b0) = unpremultiply_rgb(10, 0, 0, 10);
  assert_eq!(decoded.get_pixel(0, 0).0, [r0, g0, b0, 10]);
  let (r1, g1, b1) = unpremultiply_rgb(0, 20, 0, 40);
  assert_eq!(decoded.get_pixel(1, 0).0, [r1, g1, b1, 40]);
  let (r2, g2, b2) = unpremultiply_rgb(0, 0, 30, 60);
  assert_eq!(decoded.get_pixel(0, 1).0, [r2, g2, b2, 60]);
  let (r3, g3, b3) = unpremultiply_rgb(40, 40, 40, 40);
  assert_eq!(decoded.get_pixel(1, 1).0, [r3, g3, b3, 40]);
}

#[test]
fn jpeg_round_trip_1x1_unpremultiplies_before_dropping_alpha() {
  let _guard = lock_tests();
  let mut pixmap = Pixmap::new(1, 1).expect("pixmap");
  pixmap.data_mut().copy_from_slice(&[10, 0, 0, 10]);

  let encoded = encode_image(&pixmap, OutputFormat::Jpeg(100)).expect("jpeg encode");
  let decoded = image::load_from_memory(&encoded)
    .expect("jpeg decode")
    .to_rgb8();
  let px = decoded.get_pixel(0, 0).0;

  // If we accidentally encoded premultiplied values, we'd see something close to 10, not 255.
  assert!(px[0] > 200, "expected bright red after unpremultiply, got {:?}", px);
}

#[test]
fn jpeg_round_trip_2x2_basic_dimensions_and_pixels() {
  let _guard = lock_tests();
  let mut pixmap = Pixmap::new(2, 2).expect("pixmap");
  // Solid premultiplied pixel to keep JPEG subsampling deterministic.
  pixmap
    .data_mut()
    .copy_from_slice(&[10, 0, 0, 10, 10, 0, 0, 10, 10, 0, 0, 10, 10, 0, 0, 10]);

  let encoded = encode_image(&pixmap, OutputFormat::Jpeg(100)).expect("jpeg encode");
  let decoded = image::load_from_memory(&encoded)
    .expect("jpeg decode")
    .to_rgb8();
  assert_eq!(decoded.dimensions(), (2, 2));
  let px = decoded.get_pixel(0, 0).0;
  assert!(px[0] > 200, "expected bright red after unpremultiply, got {:?}", px);
}

#[test]
fn webp_round_trip_1x1_unpremultiplies_and_preserves_alpha() {
  let _guard = lock_tests();
  let mut pixmap = Pixmap::new(1, 1).expect("pixmap");
  pixmap.data_mut().copy_from_slice(&[10, 0, 0, 10]);

  let encoded = encode_image(&pixmap, OutputFormat::WebP(100)).expect("webp encode");
  let decoded = decode_rgba(&encoded);
  let px = decoded.get_pixel(0, 0).0;

  assert!(px[0] > 200, "expected bright red after unpremultiply, got {:?}", px);
  assert!(px[3].abs_diff(10) <= 2, "expected alpha ~10, got {:?}", px);
}

#[test]
fn webp_round_trip_2x2_basic_dimensions_and_pixels() {
  let _guard = lock_tests();
  let mut pixmap = Pixmap::new(2, 2).expect("pixmap");
  pixmap
    .data_mut()
    .copy_from_slice(&[10, 0, 0, 10, 10, 0, 0, 10, 10, 0, 0, 10, 10, 0, 0, 10]);

  let encoded = encode_image(&pixmap, OutputFormat::WebP(100)).expect("webp encode");
  let decoded = decode_rgba(&encoded);
  assert_eq!(decoded.dimensions(), (2, 2));
  let px = decoded.get_pixel(0, 0).0;
  assert!(px[0] > 200, "expected bright red after unpremultiply, got {:?}", px);
  assert!(px[3].abs_diff(10) <= 2, "expected alpha ~10, got {:?}", px);
}

#[test]
fn png_streaming_encode_avoids_full_frame_intermediate_allocations() {
  let _guard = lock_tests();
  let width = 1024;
  let height = 1024;
  let mut pixmap = Pixmap::new(width, height).expect("pixmap");
  // A solid image compresses well, keeping the output buffer small so allocator tracking is
  // dominated by intermediate scratch buffers.
  pixmap.data_mut().fill(0);

  reset_max_alloc();
  let encoded = encode_image(&pixmap, OutputFormat::Png).expect("png encode");
  assert!(!encoded.is_empty());

  let max = max_alloc();
  assert!(
    max < 1024 * 1024,
    "expected PNG streaming path to avoid multi-megabyte intermediate allocations; max allocation was {max} bytes"
  );

  // Spot-check that the encoded bytes represent the expected dimensions.
  let decoded = image::load_from_memory(&encoded).expect("decode png");
  assert_eq!(decoded.dimensions(), (width, height));
}

#[test]
fn jpeg_streaming_encode_avoids_full_frame_intermediate_allocations() {
  let _guard = lock_tests();
  let width = 1024;
  let height = 1024;
  let mut pixmap = Pixmap::new(width, height).expect("pixmap");
  pixmap.data_mut().fill(0);

  reset_max_alloc();
  let encoded = encode_image(&pixmap, OutputFormat::Jpeg(80)).expect("jpeg encode");
  assert!(!encoded.is_empty());

  let max = max_alloc();
  assert!(
    max < 2 * 1024 * 1024,
    "expected JPEG streaming path to avoid full-frame RGB intermediate allocations; max allocation was {max} bytes"
  );
}

#[test]
fn webp_encode_avoids_full_frame_intermediate_allocations() {
  let _guard = lock_tests();
  let width = 1024;
  let height = 1024;
  let mut pixmap = Pixmap::new(width, height).expect("pixmap");
  pixmap.data_mut().fill(0);

  reset_max_alloc();
  let encoded = encode_image(&pixmap, OutputFormat::WebP(80)).expect("webp encode");
  assert!(!encoded.is_empty());

  let max = max_alloc();
  assert!(
    max < 2 * 1024 * 1024,
    "expected WebP encoder to avoid allocating a full-frame RGBA buffer in Rust; max allocation was {max} bytes"
  );
}
