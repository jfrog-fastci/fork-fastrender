use crate::error::{Error, RenderError};
use crate::fallible_vec_writer::FallibleVecWriter;
use crate::paint::pixmap::reserve_buffer;
use crate::paint::pixmap::MAX_PIXMAP_BYTES;
use image::{Rgba, RgbaImage};
use std::io::Cursor;
use std::io::Write;

/// Configuration for comparing two images.
#[derive(Debug, Clone)]
pub struct CompareConfig {
  /// Maximum allowed difference per color channel (0-255).
  pub channel_tolerance: u8,
  /// Maximum percentage of pixels that can differ before failing (0.0-100.0).
  pub max_different_percent: f64,
  /// Whether to compare the alpha channel. When false, alpha is ignored.
  pub compare_alpha: bool,
  /// Whether to generate a visual diff image.
  pub generate_diff_image: bool,
  /// Optional perceptual distance threshold (0.0 = identical, higher = more different).
  pub max_perceptual_distance: Option<f64>,
}

impl Default for CompareConfig {
  fn default() -> Self {
    Self {
      channel_tolerance: 0,
      max_different_percent: 0.0,
      compare_alpha: true,
      generate_diff_image: true,
      max_perceptual_distance: None,
    }
  }
}

impl CompareConfig {
  /// Strict comparison: exact match required.
  pub fn strict() -> Self {
    Self::default()
  }

  /// Lenient comparison: allow minor AA/rounding differences.
  pub fn lenient() -> Self {
    Self {
      channel_tolerance: 5,
      max_different_percent: 0.1,
      compare_alpha: true,
      generate_diff_image: true,
      max_perceptual_distance: Some(0.02),
    }
  }

  /// Fuzzy comparison: allow small raster differences and ignore alpha.
  pub fn fuzzy() -> Self {
    Self {
      channel_tolerance: 10,
      max_different_percent: 1.0,
      compare_alpha: false,
      generate_diff_image: true,
      max_perceptual_distance: Some(0.05),
    }
  }

  /// Sets the channel tolerance.
  pub fn with_channel_tolerance(mut self, tolerance: u8) -> Self {
    self.channel_tolerance = tolerance;
    self
  }

  /// Sets the max different percent.
  pub fn with_max_different_percent(mut self, percent: f64) -> Self {
    self.max_different_percent = percent;
    self
  }

  /// Enables or disables alpha comparison.
  pub fn with_compare_alpha(mut self, compare: bool) -> Self {
    self.compare_alpha = compare;
    self
  }

  /// Enables or disables diff image generation.
  pub fn with_generate_diff_image(mut self, generate: bool) -> Self {
    self.generate_diff_image = generate;
    self
  }

  /// Sets the maximum perceptual distance allowed for a pass.
  pub fn with_max_perceptual_distance(mut self, max_distance: Option<f64>) -> Self {
    self.max_perceptual_distance = max_distance;
    self
  }
}

/// Statistics about pixel and perceptual differences.
#[derive(Debug, Clone, Default)]
pub struct DiffStatistics {
  /// Total number of pixels compared.
  pub total_pixels: u64,
  /// Number of pixels that differ (respecting tolerance/alpha settings).
  pub different_pixels: u64,
  /// Percentage of pixels that differ (0.0-100.0).
  pub different_percent: f64,
  /// Maximum difference in red channel.
  pub max_red_diff: u8,
  /// Maximum difference in green channel.
  pub max_green_diff: u8,
  /// Maximum difference in blue channel.
  pub max_blue_diff: u8,
  /// Maximum difference in alpha channel.
  pub max_alpha_diff: u8,
  /// Mean squared error across compared channels.
  pub mse: f64,
  /// Peak signal-to-noise ratio (higher is closer).
  pub psnr: f64,
  /// Perceptual similarity (1.0 = identical).
  pub perceptual_similarity: f64,
  /// Perceptual distance (0.0 = identical).
  pub perceptual_distance: f64,
}

impl DiffStatistics {
  /// Returns the maximum difference across all channels.
  pub fn max_channel_diff(&self, compare_alpha: bool) -> u8 {
    let max_rgb = self
      .max_red_diff
      .max(self.max_green_diff)
      .max(self.max_blue_diff);
    if compare_alpha {
      max_rgb.max(self.max_alpha_diff)
    } else {
      max_rgb
    }
  }
}

/// Result of comparing two images.
#[derive(Debug, Clone)]
pub struct ImageDiff {
  /// Whether the images match according to the config.
  pub matches: bool,
  /// Statistics about the differences.
  pub statistics: DiffStatistics,
  /// Difference image (red highlights), if generated.
  pub diff_image: Option<RgbaImage>,
  /// Whether dimensions matched.
  pub dimensions_match: bool,
  /// Actual image dimensions.
  pub actual_dimensions: (u32, u32),
  /// Expected image dimensions.
  pub expected_dimensions: (u32, u32),
  /// Comparison configuration used to generate this diff.
  pub config: CompareConfig,
}

impl ImageDiff {
  /// Returns true if the images match according to the comparison config.
  pub fn is_match(&self) -> bool {
    self.matches
  }

  /// Saves the diff image (if generated) to the given path.
  pub fn save_diff_image(&self, path: &std::path::Path) -> Result<(), String> {
    if let Some(ref diff) = self.diff_image {
      diff
        .save(path)
        .map_err(|e| format!("Failed to save diff image: {}", e))
    } else {
      Err("No diff image generated".to_string())
    }
  }

  /// Returns a human-readable summary of the comparison result.
  pub fn summary(&self) -> String {
    if !self.dimensions_match {
      return format!(
        "Dimension mismatch: actual {}x{}, expected {}x{}",
        self.actual_dimensions.0,
        self.actual_dimensions.1,
        self.expected_dimensions.0,
        self.expected_dimensions.1
      );
    }

    if self.matches {
      return format!(
        "Images match ({:.4}% different, perceptual distance {:.4})",
        self.statistics.different_percent, self.statistics.perceptual_distance
      );
    }

    format!(
      "Images differ: {} of {} pixels ({:.4}%), max channel diff: {}, perceptual distance {:.4}, PSNR: {:.2} dB",
      self.statistics.different_pixels,
      self.statistics.total_pixels,
      self.statistics.different_percent,
      self.statistics.max_channel_diff(self.config.compare_alpha),
      self.statistics.perceptual_distance,
      self.statistics.psnr
    )
  }

  /// Encodes the diff image (if present) to PNG bytes.
  pub fn diff_png(&self) -> Result<Option<Vec<u8>>, Error> {
    if let Some(ref diff) = self.diff_image {
      encode_png(diff).map(Some)
    } else {
      Ok(None)
    }
  }
}

/// Compare two RGBA images according to the provided config.
pub fn compare_images(
  actual: &RgbaImage,
  expected: &RgbaImage,
  config: &CompareConfig,
) -> ImageDiff {
  let actual_dims = (actual.width(), actual.height());
  let expected_dims = (expected.width(), expected.height());

  if actual_dims != expected_dims {
    return ImageDiff {
      matches: false,
      statistics: DiffStatistics::default(),
      diff_image: None,
      dimensions_match: false,
      actual_dimensions: actual_dims,
      expected_dimensions: expected_dims,
      config: config.clone(),
    };
  }

  let width = actual.width();
  let height = actual.height();
  let total_pixels = (width as u64) * (height as u64);

  let mut diff_image = if config.generate_diff_image {
    allocate_diff_image(width, height)
  } else {
    None
  };

  let mut different_pixels = 0u64;
  let mut max_red_diff = 0u8;
  let mut max_green_diff = 0u8;
  let mut max_blue_diff = 0u8;
  let mut max_alpha_diff = 0u8;
  let mut sum_squared_error = 0.0f64;
  let mut ssim = SsimAccumulator::default();

  let tolerance = config.channel_tolerance as i16;

  for (i, (actual_px, expected_px)) in actual.pixels().zip(expected.pixels()).enumerate() {
    let diff_r = (actual_px[0] as i16 - expected_px[0] as i16).unsigned_abs() as u8;
    let diff_g = (actual_px[1] as i16 - expected_px[1] as i16).unsigned_abs() as u8;
    let diff_b = (actual_px[2] as i16 - expected_px[2] as i16).unsigned_abs() as u8;
    let diff_a = (actual_px[3] as i16 - expected_px[3] as i16).unsigned_abs() as u8;

    max_red_diff = max_red_diff.max(diff_r);
    max_green_diff = max_green_diff.max(diff_g);
    max_blue_diff = max_blue_diff.max(diff_b);
    max_alpha_diff = max_alpha_diff.max(diff_a);

    sum_squared_error += (diff_r as f64).powi(2);
    sum_squared_error += (diff_g as f64).powi(2);
    sum_squared_error += (diff_b as f64).powi(2);
    if config.compare_alpha {
      sum_squared_error += (diff_a as f64).powi(2);
    }

    let is_different = diff_r as i16 > tolerance
      || diff_g as i16 > tolerance
      || diff_b as i16 > tolerance
      || (config.compare_alpha && diff_a as i16 > tolerance);

    if is_different {
      different_pixels += 1;
    }

    if let Some(ref mut diff_img) = diff_image {
      let intensity = if is_different {
        diff_r
          .max(diff_g)
          .max(diff_b)
          .max(if config.compare_alpha { diff_a } else { 0 })
      } else {
        0
      };

      let (x, y) = (i as u32 % width, i as u32 / width);
      diff_img.put_pixel(
        x,
        y,
        if is_different {
          // Highlight differences in red with alpha scaled by magnitude.
          let alpha = intensity.saturating_mul(2).min(255);
          Rgba([255, 0, 0, alpha])
        } else {
          Rgba([0, 0, 0, 0])
        },
      );
    }

    // Perceptual metric uses luminance; optionally include alpha as a multiplier.
    let alpha_actual = if config.compare_alpha {
      actual_px[3] as f64 / 255.0
    } else {
      1.0
    };
    let alpha_expected = if config.compare_alpha {
      expected_px[3] as f64 / 255.0
    } else {
      1.0
    };

    let luma_actual =
      (0.2126 * actual_px[0] as f64 + 0.7152 * actual_px[1] as f64 + 0.0722 * actual_px[2] as f64)
        * alpha_actual;
    let luma_expected = (0.2126 * expected_px[0] as f64
      + 0.7152 * expected_px[1] as f64
      + 0.0722 * expected_px[2] as f64)
      * alpha_expected;

    ssim.push(luma_actual, luma_expected);
  }

  let different_percent = if total_pixels > 0 {
    (different_pixels as f64 / total_pixels as f64) * 100.0
  } else {
    0.0
  };

  let channels = if config.compare_alpha { 4.0 } else { 3.0 };
  let mse = if total_pixels > 0 {
    sum_squared_error / (total_pixels as f64 * channels)
  } else {
    0.0
  };
  let psnr = if mse > 0.0 {
    10.0 * (255.0f64.powi(2) / mse).log10()
  } else {
    f64::INFINITY
  };

  let perceptual_similarity = ssim.finish();
  let perceptual_distance = 1.0 - perceptual_similarity.clamp(0.0, 1.0);

  let statistics = DiffStatistics {
    total_pixels,
    different_pixels,
    different_percent,
    max_red_diff,
    max_green_diff,
    max_blue_diff,
    max_alpha_diff,
    mse,
    psnr,
    perceptual_similarity,
    perceptual_distance,
  };

  let passes_pixels = different_percent <= config.max_different_percent + f64::EPSILON;
  let passes_perceptual = config
    .max_perceptual_distance
    .map(|max| perceptual_distance <= max + f64::EPSILON)
    .unwrap_or(true);

  ImageDiff {
    matches: passes_pixels && passes_perceptual,
    statistics,
    diff_image,
    dimensions_match: true,
    actual_dimensions: actual_dims,
    expected_dimensions: expected_dims,
    config: config.clone(),
  }
}

/// Compare two PNG byte buffers.
pub fn compare_png(
  rendered: &[u8],
  expected: &[u8],
  config: &CompareConfig,
) -> Result<ImageDiff, Error> {
  let rendered_img = decode_png(rendered)?;
  let expected_img = decode_png(expected)?;
  Ok(compare_images(&rendered_img, &expected_img, config))
}

/// Decode PNG bytes into an RGBA image.
pub fn decode_png(data: &[u8]) -> Result<RgbaImage, Error> {
  let mut decoder = png::Decoder::new(Cursor::new(data));
  decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);

  let mut reader = decoder.read_info().map_err(|e| {
    Error::Render(RenderError::InvalidParameters {
      message: format!("Failed to decode PNG: {e}"),
    })
  })?;

  let info = reader.info();
  let width = info.width;
  let height = info.height;
  if width == 0 || height == 0 {
    return Err(Error::Render(RenderError::InvalidParameters {
      message: format!("Failed to decode PNG: image size is zero ({width}x{height})"),
    }));
  }

  let rgba_bytes = u64::from(width)
    .checked_mul(u64::from(height))
    .and_then(|px| px.checked_mul(4))
    .ok_or_else(|| {
      Error::Render(RenderError::InvalidParameters {
        message: format!("Failed to decode PNG: dimensions overflow ({width}x{height})"),
      })
    })?;

  if rgba_bytes > MAX_PIXMAP_BYTES {
    return Err(Error::Render(RenderError::InvalidParameters {
      message: format!(
        "Failed to decode PNG: decoded image {}x{} is {} bytes (limit {})",
        width, height, rgba_bytes, MAX_PIXMAP_BYTES
      ),
    }));
  }

  let out_size = reader.output_buffer_size().ok_or_else(|| {
    Error::Render(RenderError::InvalidParameters {
      message: "Failed to decode PNG: output buffer size not available".to_string(),
    })
  })?;
  let out_size_u64 = u64::try_from(out_size).map_err(|_| {
    Error::Render(RenderError::InvalidParameters {
      message: "Failed to decode PNG: output buffer size does not fit in u64".to_string(),
    })
  })?;

  let mut buf = reserve_buffer(out_size_u64, "decode_png: output buffer").map_err(Error::Render)?;
  buf.resize(out_size, 0);

  let frame = reader.next_frame(&mut buf).map_err(|e| {
    Error::Render(RenderError::InvalidParameters {
      message: format!("Failed to decode PNG: {e}"),
    })
  })?;
  buf.truncate(frame.buffer_size());

  let rgba_len = usize::try_from(rgba_bytes).map_err(|_| {
    Error::Render(RenderError::InvalidParameters {
      message: format!("Failed to decode PNG: decoded byte size does not fit in usize ({width}x{height})"),
    })
  })?;

  match (frame.color_type, frame.bit_depth) {
    (png::ColorType::Rgba, png::BitDepth::Eight) => {
      if buf.len() != rgba_len {
        return Err(Error::Render(RenderError::InvalidParameters {
          message: format!(
            "Failed to decode PNG: RGBA output length mismatch (expected {rgba_len} bytes, got {})",
            buf.len()
          ),
        }));
      }
      RgbaImage::from_raw(width, height, buf).ok_or_else(|| {
        Error::Render(RenderError::InvalidParameters {
          message: "Failed to decode PNG: invalid RGBA buffer".to_string(),
        })
      })
    }
    (png::ColorType::Rgb, png::BitDepth::Eight) => {
      let rgb_len = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|px| px.checked_mul(3))
        .ok_or_else(|| {
          Error::Render(RenderError::InvalidParameters {
            message: "Failed to decode PNG: RGB byte size overflow".to_string(),
          })
        })?;
      let rgb_len = usize::try_from(rgb_len).map_err(|_| {
        Error::Render(RenderError::InvalidParameters {
          message: "Failed to decode PNG: RGB byte size does not fit in usize".to_string(),
        })
      })?;
      if buf.len() != rgb_len {
        return Err(Error::Render(RenderError::InvalidParameters {
          message: format!(
            "Failed to decode PNG: RGB output length mismatch (expected {rgb_len} bytes, got {})",
            buf.len()
          ),
        }));
      }

      let mut out = reserve_buffer(rgba_bytes, "decode_png: RGBA buffer").map_err(Error::Render)?;
      out.resize(rgba_len, 0);
      for (in_px, out_px) in buf.chunks_exact(3).zip(out.chunks_exact_mut(4)) {
        out_px[0] = in_px[0];
        out_px[1] = in_px[1];
        out_px[2] = in_px[2];
        out_px[3] = 255;
      }
      RgbaImage::from_raw(width, height, out).ok_or_else(|| {
        Error::Render(RenderError::InvalidParameters {
          message: "Failed to decode PNG: invalid RGB->RGBA buffer".to_string(),
        })
      })
    }
    (png::ColorType::Grayscale, png::BitDepth::Eight) => {
      let gray_len = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| {
          Error::Render(RenderError::InvalidParameters {
            message: "Failed to decode PNG: grayscale byte size overflow".to_string(),
          })
        })?;
      let gray_len = usize::try_from(gray_len).map_err(|_| {
        Error::Render(RenderError::InvalidParameters {
          message: "Failed to decode PNG: grayscale byte size does not fit in usize".to_string(),
        })
      })?;
      if buf.len() != gray_len {
        return Err(Error::Render(RenderError::InvalidParameters {
          message: format!(
            "Failed to decode PNG: grayscale output length mismatch (expected {gray_len} bytes, got {})",
            buf.len()
          ),
        }));
      }

      let mut out = reserve_buffer(rgba_bytes, "decode_png: RGBA buffer").map_err(Error::Render)?;
      out.resize(rgba_len, 0);
      for (gray, out_px) in buf.iter().zip(out.chunks_exact_mut(4)) {
        out_px[0] = *gray;
        out_px[1] = *gray;
        out_px[2] = *gray;
        out_px[3] = 255;
      }
      RgbaImage::from_raw(width, height, out).ok_or_else(|| {
        Error::Render(RenderError::InvalidParameters {
          message: "Failed to decode PNG: invalid grayscale->RGBA buffer".to_string(),
        })
      })
    }
    (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight) => {
      let ga_len = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|px| px.checked_mul(2))
        .ok_or_else(|| {
          Error::Render(RenderError::InvalidParameters {
            message: "Failed to decode PNG: grayscale-alpha byte size overflow".to_string(),
          })
        })?;
      let ga_len = usize::try_from(ga_len).map_err(|_| {
        Error::Render(RenderError::InvalidParameters {
          message: "Failed to decode PNG: grayscale-alpha byte size does not fit in usize".to_string(),
        })
      })?;
      if buf.len() != ga_len {
        return Err(Error::Render(RenderError::InvalidParameters {
          message: format!(
            "Failed to decode PNG: grayscale-alpha output length mismatch (expected {ga_len} bytes, got {})",
            buf.len()
          ),
        }));
      }

      let mut out = reserve_buffer(rgba_bytes, "decode_png: RGBA buffer").map_err(Error::Render)?;
      out.resize(rgba_len, 0);
      for (in_px, out_px) in buf.chunks_exact(2).zip(out.chunks_exact_mut(4)) {
        let gray = in_px[0];
        out_px[0] = gray;
        out_px[1] = gray;
        out_px[2] = gray;
        out_px[3] = in_px[1];
      }
      RgbaImage::from_raw(width, height, out).ok_or_else(|| {
        Error::Render(RenderError::InvalidParameters {
          message: "Failed to decode PNG: invalid grayscale-alpha->RGBA buffer".to_string(),
        })
      })
    }
    (color_type, bit_depth) => Err(Error::Render(RenderError::InvalidParameters {
      message: format!("Failed to decode PNG: unsupported PNG format ({color_type:?} {bit_depth:?})"),
    })),
  }
}

/// Encode an RGBA image to PNG bytes.
pub fn encode_png(image: &RgbaImage) -> Result<Vec<u8>, Error> {
  let width = image.width();
  let height = image.height();

  if width == 0 || height == 0 {
    return Err(Error::Render(RenderError::InvalidParameters {
      message: format!("encode_png: image size is zero ({width}x{height})"),
    }));
  }

  let row_len = (width as usize).checked_mul(4).ok_or_else(|| {
    Error::Render(RenderError::InvalidParameters {
      message: format!("encode_png: row byte size overflow (width={width})"),
    })
  })?;

  let expected_len = row_len.checked_mul(height as usize).ok_or_else(|| {
    Error::Render(RenderError::InvalidParameters {
      message: format!("encode_png: image byte size overflow ({width}x{height})"),
    })
  })?;

  let pixels = image.as_raw();
  if pixels.len() != expected_len {
    return Err(Error::Render(RenderError::InvalidParameters {
      message: format!(
        "encode_png: image data length mismatch (expected {expected_len} bytes, got {})",
        pixels.len()
      ),
    }));
  }

  let mut buffer = FallibleVecWriter::new(MAX_PIXMAP_BYTES as usize, "encode_png: PNG output");
  {
    let mut encoder = png::Encoder::new(&mut buffer, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);

    let mut writer = encoder.write_header().map_err(|e| {
      Error::Render(RenderError::EncodeFailed {
        format: "PNG".to_string(),
        reason: e.to_string(),
      })
    })?;
    let mut stream = writer.stream_writer().map_err(|e| {
      Error::Render(RenderError::EncodeFailed {
        format: "PNG".to_string(),
        reason: e.to_string(),
      })
    })?;

    for row in pixels.chunks_exact(row_len) {
      stream.write_all(row).map_err(|e| {
        Error::Render(RenderError::EncodeFailed {
          format: "PNG".to_string(),
          reason: e.to_string(),
        })
      })?;
    }

    stream.finish().map_err(|e| {
      Error::Render(RenderError::EncodeFailed {
        format: "PNG".to_string(),
        reason: e.to_string(),
      })
    })?;
  }

  Ok(buffer.into_inner())
}

#[derive(Debug, Clone, Copy, Default)]
struct SsimAccumulator {
  n: u64,
  mean_actual: f64,
  mean_expected: f64,
  m2_actual: f64,
  m2_expected: f64,
  c: f64,
}

impl SsimAccumulator {
  fn push(&mut self, actual: f64, expected: f64) {
    self.n += 1;
    let n = self.n as f64;

    // Welford update for means, variances, and covariance (population form).
    let delta_actual = actual - self.mean_actual;
    let delta_expected = expected - self.mean_expected;

    self.mean_actual += delta_actual / n;
    self.mean_expected += delta_expected / n;

    self.m2_actual += delta_actual * (actual - self.mean_actual);
    self.m2_expected += delta_expected * (expected - self.mean_expected);
    self.c += delta_actual * (expected - self.mean_expected);
  }

  fn finish(self) -> f64 {
    if self.n == 0 {
      return 1.0;
    }

    let n = self.n as f64;
    let variance_actual = self.m2_actual / n;
    let variance_expected = self.m2_expected / n;
    let covariance = self.c / n;

    let c1 = (0.01f64 * 255.0f64).powi(2);
    let c2 = (0.03f64 * 255.0f64).powi(2);

    let numerator = (2.0 * self.mean_actual * self.mean_expected + c1) * (2.0 * covariance + c2);
    let denominator = (self.mean_actual.powi(2) + self.mean_expected.powi(2) + c1)
      * (variance_actual + variance_expected + c2);

    if denominator == 0.0 {
      1.0
    } else {
      (numerator / denominator).clamp(-1.0, 1.0)
    }
  }
}

fn allocate_diff_image(width: u32, height: u32) -> Option<RgbaImage> {
  let bytes = u64::from(width)
    .checked_mul(u64::from(height))?
    .checked_mul(4)?;
  if bytes > MAX_PIXMAP_BYTES {
    return None;
  }

  let len = usize::try_from(bytes).ok()?;
  let mut buf = Vec::new();
  buf.try_reserve_exact(len).ok()?;
  buf.resize(len, 0);
  RgbaImage::from_raw(width, height, buf)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn solid(color: [u8; 4]) -> RgbaImage {
    RgbaImage::from_pixel(2, 2, Rgba(color))
  }

  #[test]
  fn identical_images_match_strict() {
    let img = solid([10, 20, 30, 255]);
    let diff = compare_images(&img, &img, &CompareConfig::strict());
    assert!(diff.is_match());
    assert_eq!(diff.statistics.different_pixels, 0);
    assert_eq!(diff.statistics.perceptual_distance, 0.0);
  }

  #[test]
  fn detects_single_pixel_difference() {
    let mut a = solid([0, 0, 0, 255]);
    a.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
    let b = solid([0, 0, 0, 255]);

    let diff = compare_images(&a, &b, &CompareConfig::strict());
    assert!(!diff.is_match());
    assert_eq!(diff.statistics.different_pixels, 1);
    assert_eq!(diff.statistics.different_percent, 25.0);
    assert!(diff.statistics.perceptual_distance > 0.0);
    assert!(diff.diff_image.is_some());
  }

  #[test]
  fn ignores_alpha_when_configured() {
    let img_a = solid([100, 100, 100, 255]);
    let img_b = solid([100, 100, 100, 10]);

    let strict = compare_images(&img_a, &img_b, &CompareConfig::strict());
    assert!(!strict.is_match());

    let ignore_alpha = CompareConfig::strict().with_compare_alpha(false);
    let diff = compare_images(&img_a, &img_b, &ignore_alpha);
    assert!(diff.is_match());
    assert!(diff.statistics.perceptual_distance < 0.0001);
  }

  #[test]
  fn enforces_perceptual_threshold() {
    let img_a = solid([0, 0, 0, 255]);
    let img_b = solid([40, 40, 40, 255]);

    // High tolerance removes pixel diffs, but perceptual distance should still fail.
    let config = CompareConfig::strict()
      .with_channel_tolerance(255)
      .with_max_different_percent(0.0)
      .with_max_perceptual_distance(Some(0.02));

    let diff = compare_images(&img_a, &img_b, &config);
    assert!(!diff.is_match());
    assert_eq!(diff.statistics.different_pixels, 0);
    assert!(diff.statistics.perceptual_distance > 0.02);
  }

  #[test]
  fn decode_and_encode_png_round_trip() {
    let img = solid([5, 6, 7, 8]);
    let encoded = encode_png(&img).unwrap();
    let decoded = decode_png(&encoded).unwrap();
    assert_eq!(decoded.get_pixel(0, 0), img.get_pixel(0, 0));
  }

  #[test]
  fn allocate_diff_image_rejects_oversized_buffers() {
    assert!(allocate_diff_image(u32::MAX, u32::MAX).is_none());
  }
}
