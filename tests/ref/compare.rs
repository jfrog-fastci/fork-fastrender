//! Image comparison utilities for reference-style tests.
//!
//! This module wraps the shared `fastrender::image_compare` APIs with helpers
//! for converting to/from tiny-skia pixmaps used by the renderer.

use fastrender::image_compare::{compare_images as compare_rgba_images, decode_png, encode_png};
use image::RgbaImage;
use std::path::Path;
use tiny_skia::Pixmap;

pub use fastrender::image_compare::{CompareConfig, DiffStatistics, ImageDiff};

/// Compare two pixmaps using the shared image comparison module.
pub fn compare_images(actual: &Pixmap, expected: &Pixmap, config: &CompareConfig) -> ImageDiff {
  let actual_rgba = pixmap_to_rgba_image(actual);
  let expected_rgba = pixmap_to_rgba_image(expected);
  compare_rgba_images(&actual_rgba, &expected_rgba, config)
}

/// Load a PNG image from disk into a pixmap.
pub fn load_png(path: &Path) -> Result<Pixmap, String> {
  let rgba = decode_png(
    &std::fs::read(path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?,
  )
  .map_err(|e| e.to_string())?;
  pixmap_from_rgba_image(rgba)
}

/// Load a PNG from bytes into a pixmap.
pub fn load_png_from_bytes(data: &[u8]) -> Result<Pixmap, String> {
  let rgba = decode_png(data).map_err(|e| e.to_string())?;
  pixmap_from_rgba_image(rgba)
}

/// Save a pixmap as a PNG file.
#[allow(dead_code)]
pub fn save_png(pixmap: &Pixmap, path: &Path) -> Result<(), String> {
  pixmap
    .save_png(path)
    .map_err(|e| format!("Failed to save PNG '{}': {}", path.display(), e))
}

/// Create a solid color pixmap (premultiplied RGBA).
pub fn create_solid_pixmap(width: u32, height: u32, r: u8, g: u8, b: u8, a: u8) -> Option<Pixmap> {
  let mut pixmap = Pixmap::new(width, height)?;

  let alpha = a as f32 / 255.0;
  let pm_r = (r as f32 * alpha) as u8;
  let pm_g = (g as f32 * alpha) as u8;
  let pm_b = (b as f32 * alpha) as u8;

  for chunk in pixmap.data_mut().chunks_exact_mut(4) {
    chunk[0] = pm_r;
    chunk[1] = pm_g;
    chunk[2] = pm_b;
    chunk[3] = a;
  }

  Some(pixmap)
}

fn pixmap_to_rgba_image(pixmap: &Pixmap) -> RgbaImage {
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

fn pixmap_from_rgba_image(rgba: image::RgbaImage) -> Result<Pixmap, String> {
  let width = rgba.width();
  let height = rgba.height();

  let mut pixmap = Pixmap::new(width, height)
    .ok_or_else(|| format!("Failed to create pixmap {}x{}", width, height))?;

  let src_data = rgba.as_raw();
  let dst_data = pixmap.data_mut();

  for i in 0..(width * height) as usize {
    let src_idx = i * 4;
    let dst_idx = i * 4;

    let r = src_data[src_idx];
    let g = src_data[src_idx + 1];
    let b = src_data[src_idx + 2];
    let a = src_data[src_idx + 3];

    let alpha = a as f32 / 255.0;
    let pm_r = (r as f32 * alpha) as u8;
    let pm_g = (g as f32 * alpha) as u8;
    let pm_b = (b as f32 * alpha) as u8;

    dst_data[dst_idx] = pm_r;
    dst_data[dst_idx + 1] = pm_g;
    dst_data[dst_idx + 2] = pm_b;
    dst_data[dst_idx + 3] = a;
  }

  Ok(pixmap)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn compare_identical_images() {
    let config = CompareConfig::strict();
    let pixmap1 = create_solid_pixmap(4, 4, 255, 0, 0, 255).unwrap();
    let pixmap2 = create_solid_pixmap(4, 4, 255, 0, 0, 255).unwrap();

    let diff = compare_images(&pixmap1, &pixmap2, &config);
    assert!(diff.is_match());
    assert!(diff.dimensions_match);
    assert_eq!(diff.statistics.different_pixels, 0);
    assert_eq!(diff.statistics.different_percent, 0.0);
    assert_eq!(diff.statistics.max_channel_diff(config.compare_alpha), 0);
    assert!(diff.statistics.psnr.is_infinite());
    assert!(diff.statistics.perceptual_distance < 0.0001);
  }

  #[test]
  fn compare_different_images() {
    let pixmap1 = create_solid_pixmap(2, 2, 255, 0, 0, 255).unwrap();
    let pixmap2 = create_solid_pixmap(2, 2, 0, 255, 0, 255).unwrap();

    let diff = compare_images(&pixmap1, &pixmap2, &CompareConfig::strict());
    assert!(!diff.is_match());
    assert!(diff.dimensions_match);
    assert_eq!(diff.statistics.different_pixels, 4);
    assert_eq!(diff.statistics.different_percent, 100.0);
    assert_eq!(diff.statistics.max_red_diff, 255);
    assert_eq!(diff.statistics.max_green_diff, 255);
    assert!(diff.statistics.perceptual_distance > 0.1);
    assert!(diff.diff_image.is_some());
  }

  #[test]
  fn compare_with_tolerance_and_percent() {
    let expected = create_solid_pixmap(10, 10, 100, 100, 100, 255).unwrap();
    let mut actual = create_solid_pixmap(10, 10, 100, 100, 100, 255).unwrap();
    actual.data_mut()[0] = 0;

    let strict = compare_images(&actual, &expected, &CompareConfig::strict());
    assert!(!strict.is_match());

    let config = CompareConfig::strict()
      .with_channel_tolerance(5)
      .with_max_different_percent(1.1);
    let diff = compare_images(&actual, &expected, &config);

    assert!(diff.is_match());
    assert_eq!(diff.statistics.different_pixels, 1);
  }

  #[test]
  fn config_presets_include_perceptual_limits() {
    let strict = CompareConfig::strict();
    assert_eq!(strict.channel_tolerance, 0);
    assert_eq!(strict.max_different_percent, 0.0);

    let lenient = CompareConfig::lenient();
    assert_eq!(lenient.channel_tolerance, 5);
    assert_eq!(lenient.max_different_percent, 0.1);
    assert!(lenient.max_perceptual_distance.is_some());

    let fuzzy = CompareConfig::fuzzy();
    assert_eq!(fuzzy.channel_tolerance, 10);
    assert_eq!(fuzzy.max_different_percent, 1.0);
    assert!(!fuzzy.compare_alpha);
    assert!(fuzzy.max_perceptual_distance.is_some());
  }

  #[test]
  fn load_png_round_trip() {
    let pixmap = create_solid_pixmap(2, 2, 50, 60, 70, 255).unwrap();
    let buffer = encode_png(&pixmap_to_rgba_image(&pixmap)).expect("failed to encode test png");

    let loaded = load_png_from_bytes(&buffer).expect("failed to load png");
    let diff = compare_images(&pixmap, &loaded, &CompareConfig::strict());
    assert!(diff.is_match());
  }

  #[test]
  fn compare_different_dimensions() {
    let pixmap1 = create_solid_pixmap(10, 10, 255, 0, 0, 255).unwrap();
    let pixmap2 = create_solid_pixmap(20, 20, 255, 0, 0, 255).unwrap();

    let diff = compare_images(&pixmap1, &pixmap2, &CompareConfig::strict());

    assert!(!diff.is_match());
    assert!(!diff.dimensions_match);
    assert_eq!(diff.actual_dimensions, (10, 10));
    assert_eq!(diff.expected_dimensions, (20, 20));
  }

  #[test]
  fn diff_image_generation_respects_flag() {
    let pixmap1 = create_solid_pixmap(10, 10, 255, 0, 0, 255).unwrap();
    let pixmap2 = create_solid_pixmap(10, 10, 0, 255, 0, 255).unwrap();

    let config_with_diff = CompareConfig::default().with_generate_diff_image(true);
    let diff = compare_images(&pixmap1, &pixmap2, &config_with_diff);
    assert!(diff.diff_image.is_some());

    let config_without_diff = CompareConfig::default().with_generate_diff_image(false);
    let diff2 = compare_images(&pixmap1, &pixmap2, &config_without_diff);
    assert!(diff2.diff_image.is_none());
  }

  #[test]
  fn diff_summary_mentions_dimension_mismatch() {
    let pixmap1 = create_solid_pixmap(10, 10, 255, 0, 0, 255).unwrap();
    let pixmap2 = create_solid_pixmap(10, 10, 255, 0, 0, 255).unwrap();

    let diff = compare_images(&pixmap1, &pixmap2, &CompareConfig::strict());
    assert_eq!(
      diff.summary(),
      "Images match (0.0000% different, perceptual distance 0.0000)"
    );

    let pixmap3 = create_solid_pixmap(20, 20, 255, 0, 0, 255).unwrap();
    let diff2 = compare_images(&pixmap1, &pixmap3, &CompareConfig::strict());
    assert!(diff2.summary().contains("Dimension mismatch"));
  }

  #[test]
  fn create_solid_pixmap_preserves_premultiplied_rgba() {
    let pixmap = create_solid_pixmap(5, 5, 128, 64, 32, 255).unwrap();

    assert_eq!(pixmap.width(), 5);
    assert_eq!(pixmap.height(), 5);

    let data = pixmap.data();
    assert_eq!(data[0], 128);
    assert_eq!(data[1], 64);
    assert_eq!(data[2], 32);
    assert_eq!(data[3], 255);
  }

  #[test]
  fn psnr_calculation_matches_identical_vs_different() {
    let pixmap1 = create_solid_pixmap(10, 10, 128, 128, 128, 255).unwrap();
    let pixmap2 = create_solid_pixmap(10, 10, 128, 128, 128, 255).unwrap();

    let diff = compare_images(&pixmap1, &pixmap2, &CompareConfig::strict());
    assert!(diff.statistics.psnr.is_infinite());

    let pixmap3 = create_solid_pixmap(10, 10, 0, 0, 0, 255).unwrap();
    let diff2 = compare_images(&pixmap1, &pixmap3, &CompareConfig::strict());
    assert!(diff2.statistics.psnr.is_finite());
    assert!(diff2.statistics.psnr > 0.0);
  }

  #[test]
  fn mse_calculation_matches_identical_vs_different() {
    let pixmap1 = create_solid_pixmap(10, 10, 128, 128, 128, 255).unwrap();
    let pixmap2 = create_solid_pixmap(10, 10, 128, 128, 128, 255).unwrap();

    let diff = compare_images(&pixmap1, &pixmap2, &CompareConfig::strict());
    assert_eq!(diff.statistics.mse, 0.0);

    let pixmap3 = create_solid_pixmap(10, 10, 0, 0, 0, 255).unwrap();
    let diff2 = compare_images(&pixmap1, &pixmap3, &CompareConfig::strict());
    assert!(diff2.statistics.mse > 0.0);
  }

  #[test]
  fn statistics_max_channel_diff_includes_alpha_when_requested() {
    let stats = DiffStatistics {
      total_pixels: 100,
      different_pixels: 10,
      different_percent: 10.0,
      max_red_diff: 50,
      max_green_diff: 100,
      max_blue_diff: 25,
      max_alpha_diff: 75,
      mse: 0.0,
      psnr: 0.0,
      perceptual_similarity: 0.0,
      perceptual_distance: 0.0,
      first_mismatch: None,
      first_mismatch_rgba: None,
    };

    assert_eq!(stats.max_channel_diff(true), 100);
  }
}
