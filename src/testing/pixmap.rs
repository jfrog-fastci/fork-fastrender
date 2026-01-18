#![cfg(test)]

use crate::image_compare::{compare_images as compare_rgba_images, CompareConfig, ImageDiff};
use image::RgbaImage;
use tiny_skia::Pixmap;

/// Convert a tiny-skia pixmap (premultiplied RGBA) into a normal RGBA image.
pub(crate) fn pixmap_to_rgba_image(pixmap: &Pixmap) -> RgbaImage {
  let width = pixmap.width();
  let height = pixmap.height();
  let mut rgba = RgbaImage::new(width, height);

  for (dst, src) in rgba
    .as_mut()
    .chunks_exact_mut(4)
    .zip(pixmap.data().chunks_exact(4))
  {
    let r = src[0];
    let g = src[1];
    let b = src[2];
    let a = src[3];

    if a == 0 {
      dst.copy_from_slice(&[0, 0, 0, 0]);
      continue;
    }

    let alpha = a as f32 / 255.0;
    dst[0] = ((r as f32 / alpha).min(255.0)) as u8;
    dst[1] = ((g as f32 / alpha).min(255.0)) as u8;
    dst[2] = ((b as f32 / alpha).min(255.0)) as u8;
    dst[3] = a;
  }

  rgba
}

/// Convert a normal RGBA image into a tiny-skia pixmap (premultiplied RGBA).
pub(crate) fn pixmap_from_rgba_image(rgba: &RgbaImage) -> Result<Pixmap, String> {
  let width = rgba.width();
  let height = rgba.height();

  let mut pixmap = Pixmap::new(width, height)
    .ok_or_else(|| format!("Failed to create pixmap {}x{}", width, height))?;

  let src_data = rgba.as_raw();
  let dst_data = pixmap.data_mut();

  for (src, dst) in src_data.chunks_exact(4).zip(dst_data.chunks_exact_mut(4)) {
    let r = src[0];
    let g = src[1];
    let b = src[2];
    let a = src[3];

    let alpha = a as f32 / 255.0;
    dst[0] = (r as f32 * alpha) as u8;
    dst[1] = (g as f32 * alpha) as u8;
    dst[2] = (b as f32 * alpha) as u8;
    dst[3] = a;
  }

  Ok(pixmap)
}

/// Compare two pixmaps using the shared crate image comparison implementation.
pub(crate) fn compare_pixmaps(actual: &Pixmap, expected: &Pixmap, config: &CompareConfig) -> ImageDiff {
  let actual_rgba = pixmap_to_rgba_image(actual);
  let expected_rgba = pixmap_to_rgba_image(expected);
  compare_rgba_images(&actual_rgba, &expected_rgba, config)
}

/// Assert that two pixmaps match and emit a helpful mismatch summary on failure.
pub(crate) fn assert_pixmap_eq(label: &str, expected: &Pixmap, actual: &Pixmap, config: &CompareConfig) {
  let actual_rgba = pixmap_to_rgba_image(actual);
  let expected_rgba = pixmap_to_rgba_image(expected);
  let diff = compare_rgba_images(&actual_rgba, &expected_rgba, config);
  if diff.is_match() {
    return;
  }

  let mut message = format!("Pixmap mismatch for '{label}': {}", diff.summary());
  if let Some((x, y)) = diff.statistics.first_mismatch {
    message.push_str(&format!("\nFirst mismatch at ({x}, {y})"));
    if let Some((actual_px, expected_px)) = diff.statistics.first_mismatch_rgba {
      message.push_str(&format!(
        "\nActual RGBA: {:?}\nExpected RGBA: {:?}",
        actual_px, expected_px
      ));
    }
  }

  if let Some((min_x, min_y, max_x, max_y)) = mismatch_bounding_box(&actual_rgba, &expected_rgba, config) {
    message.push_str(&format!(
      "\nMismatch bounding box: x={min_x}..={max_x}, y={min_y}..={max_y}"
    ));
  }

  assert!(false, "{message}");
}

fn mismatch_bounding_box(
  actual: &RgbaImage,
  expected: &RgbaImage,
  config: &CompareConfig,
) -> Option<(u32, u32, u32, u32)> {
  if actual.dimensions() != expected.dimensions() {
    return None;
  }

  let width = actual.width();
  let height = actual.height();
  let tolerance = config.channel_tolerance as i16;

  let mut min_x = u32::MAX;
  let mut min_y = u32::MAX;
  let mut max_x = 0u32;
  let mut max_y = 0u32;
  let mut seen = false;

  for (idx, (actual_px, expected_px)) in actual
    .as_raw()
    .chunks_exact(4)
    .zip(expected.as_raw().chunks_exact(4))
    .enumerate()
  {
    let diff_r = (actual_px[0] as i16 - expected_px[0] as i16).unsigned_abs() as u8;
    let diff_g = (actual_px[1] as i16 - expected_px[1] as i16).unsigned_abs() as u8;
    let diff_b = (actual_px[2] as i16 - expected_px[2] as i16).unsigned_abs() as u8;
    let diff_a = (actual_px[3] as i16 - expected_px[3] as i16).unsigned_abs() as u8;

    let is_different = diff_r as i16 > tolerance
      || diff_g as i16 > tolerance
      || diff_b as i16 > tolerance
      || (config.compare_alpha && diff_a as i16 > tolerance);

    if !is_different {
      continue;
    }

    let x = idx as u32 % width;
    let y = idx as u32 / width;
    if y >= height {
      break;
    }

    seen = true;
    min_x = min_x.min(x);
    min_y = min_y.min(y);
    max_x = max_x.max(x);
    max_y = max_y.max(y);
  }

  if seen {
    Some((min_x, min_y, max_x, max_y))
  } else {
    None
  }
}
