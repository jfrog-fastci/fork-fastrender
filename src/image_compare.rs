use crate::error::{Error, RenderError};
use crate::fallible_vec_writer::FallibleVecWriter;
use crate::paint::pixmap::reserve_buffer;
use crate::paint::pixmap::MAX_PIXMAP_BYTES;
use image::{Rgba, RgbaImage};
use std::io::Cursor;
use std::io::Write;

/// Stable identifier for the perceptual distance metric used by this crate.
///
/// This value is embedded in diff reports (e.g. `diff_renders` JSON/HTML output) so that reports
/// remain self-describing even if the perceptual distance implementation changes over time.
pub const PERCEPTUAL_METRIC_ID: &str = "ssim_luma_v1";

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
  /// Location (x,y) of the first pixel that exceeded the configured tolerance.
  ///
  /// The scan order is row-major (left-to-right, top-to-bottom).
  pub first_mismatch: Option<(u32, u32)>,
  /// RGBA samples at [`Self::first_mismatch`], stored as `(actual, expected)`.
  pub first_mismatch_rgba: Option<([u8; 4], [u8; 4])>,
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
  let mut first_mismatch: Option<(u32, u32)> = None;
  let mut first_mismatch_rgba: Option<([u8; 4], [u8; 4])> = None;
  let mut sum_squared_error = 0.0f64;

  // Perceptual metric (SSIM-derived) is computed on a downsampled luminance grid so it stays
  // informative for real-world diffs without allocating full-frame floating point buffers.
  let mut luma_downsample = DownsampledLumaPair::new(width, height);

  let tolerance = config.channel_tolerance as i16;

  let mut x = 0u32;
  let mut y = 0u32;
  for (actual_px, expected_px) in actual.pixels().zip(expected.pixels()) {
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
      if first_mismatch.is_none() {
        first_mismatch = Some((x, y));
        first_mismatch_rgba = Some((
          [actual_px[0], actual_px[1], actual_px[2], actual_px[3]],
          [
            expected_px[0],
            expected_px[1],
            expected_px[2],
            expected_px[3],
          ],
        ));
      }
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

    let luma_actual = rgba_luma(actual_px, config.compare_alpha);
    let luma_expected = rgba_luma(expected_px, config.compare_alpha);
    luma_downsample.push(x, y, luma_actual, luma_expected);

    x += 1;
    if x == width {
      x = 0;
      y += 1;
    }
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

  let (ds_w, ds_h, luma_actual, luma_expected) = luma_downsample.finish();
  let perceptual_similarity =
    windowed_ssim_similarity(&luma_actual, &luma_expected, ds_w, ds_h).clamp(0.0, 1.0);
  let perceptual_distance = (1.0 - perceptual_similarity).clamp(0.0, 1.0);

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
    first_mismatch,
    first_mismatch_rgba,
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

/// Compute SSIM-derived perceptual distance over the given region of two images.
///
/// The region is interpreted as the top-left `width`×`height` rectangle. Callers must ensure
/// `width <= actual.width()`, `height <= actual.height()`, and the same for `expected`.
///
/// This helper exists so callers can compute a meaningful perceptual metric even when the two
/// images have different dimensions (e.g. by passing `min(width)`/`min(height)`).
pub fn perceptual_distance_region(
  actual: &RgbaImage,
  expected: &RgbaImage,
  compare_alpha: bool,
  width: u32,
  height: u32,
) -> f64 {
  if width == 0 || height == 0 {
    return 0.0;
  }

  let mut luma_downsample = DownsampledLumaPair::new(width, height);
  for y in 0..height {
    for x in 0..width {
      let actual_px = actual.get_pixel(x, y);
      let expected_px = expected.get_pixel(x, y);

      let luma_actual = rgba_luma(actual_px, compare_alpha);
      let luma_expected = rgba_luma(expected_px, compare_alpha);
      luma_downsample.push(x, y, luma_actual, luma_expected);
    }
  }

  let (ds_w, ds_h, luma_actual, luma_expected) = luma_downsample.finish();
  let similarity = windowed_ssim_similarity(&luma_actual, &luma_expected, ds_w, ds_h);
  (1.0 - similarity).clamp(0.0, 1.0)
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
      message: format!(
        "Failed to decode PNG: decoded byte size does not fit in usize ({width}x{height})"
      ),
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
          message: "Failed to decode PNG: grayscale-alpha byte size does not fit in usize"
            .to_string(),
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
      message: format!(
        "Failed to decode PNG: unsupported PNG format ({color_type:?} {bit_depth:?})"
      ),
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

// Perceptual SSIM parameters.
//
// The perceptual metric is computed over a downsampled luminance grid to avoid large floating-point
// allocations and to smooth out extremely high-frequency differences (e.g. subpixel text AA).
const PERCEPTUAL_MAX_DOWNSAMPLED_SIDE: u32 = 256;
const PERCEPTUAL_WINDOW_SIZE: u32 = 8;

#[inline]
fn rgba_luma(px: &Rgba<u8>, compare_alpha: bool) -> f64 {
  let alpha = if compare_alpha {
    px[3] as f64 / 255.0
  } else {
    1.0
  };
  (0.2126 * px[0] as f64 + 0.7152 * px[1] as f64 + 0.0722 * px[2] as f64) * alpha
}

fn downsample_dimensions(width: u32, height: u32) -> (u32, u32) {
  let max_side = width.max(height);
  if max_side <= PERCEPTUAL_MAX_DOWNSAMPLED_SIDE {
    return (width, height);
  }

  let max_side_u64 = u64::from(max_side);
  let target = u64::from(PERCEPTUAL_MAX_DOWNSAMPLED_SIDE);
  let out_w = ((u64::from(width) * target) / max_side_u64).max(1) as u32;
  let out_h = ((u64::from(height) * target) / max_side_u64).max(1) as u32;
  (out_w, out_h)
}

#[derive(Debug)]
struct DownsampledLumaPair {
  in_width: u32,
  in_height: u32,
  out_width: u32,
  out_height: u32,
  sum_actual: Vec<f64>,
  sum_expected: Vec<f64>,
  count: Vec<u32>,
}

impl DownsampledLumaPair {
  fn new(in_width: u32, in_height: u32) -> Self {
    let (out_width, out_height) = downsample_dimensions(in_width, in_height);
    let len = out_width
      .checked_mul(out_height)
      .and_then(|v| usize::try_from(v).ok())
      .unwrap_or(0);
    Self {
      in_width,
      in_height,
      out_width,
      out_height,
      sum_actual: vec![0.0; len],
      sum_expected: vec![0.0; len],
      count: vec![0; len],
    }
  }

  #[inline]
  fn push(&mut self, x: u32, y: u32, luma_actual: f64, luma_expected: f64) {
    debug_assert!(x < self.in_width);
    debug_assert!(y < self.in_height);

    if self.out_width == 0 || self.out_height == 0 {
      return;
    }

    // Map source pixel coordinate into the downsampled grid.
    //
    // This implements a simple area partitioning where each input pixel contributes fully to one
    // output cell. We then average by the number of contributing pixels.
    let ds_x = (u64::from(x) * u64::from(self.out_width) / u64::from(self.in_width)) as u32;
    let ds_y = (u64::from(y) * u64::from(self.out_height) / u64::from(self.in_height)) as u32;
    debug_assert!(ds_x < self.out_width);
    debug_assert!(ds_y < self.out_height);

    let idx = (u64::from(ds_y) * u64::from(self.out_width) + u64::from(ds_x)) as usize;
    self.sum_actual[idx] += luma_actual;
    self.sum_expected[idx] += luma_expected;
    self.count[idx] += 1;
  }

  fn finish(mut self) -> (u32, u32, Vec<f64>, Vec<f64>) {
    for ((sum_a, sum_b), c) in self
      .sum_actual
      .iter_mut()
      .zip(self.sum_expected.iter_mut())
      .zip(self.count.iter())
    {
      if *c == 0 {
        // Should be unreachable due to the coordinate mapping, but keep the function total and
        // deterministic even for odd dimension combinations.
        *sum_a = 0.0;
        *sum_b = 0.0;
        continue;
      }
      let inv = 1.0 / (*c as f64);
      *sum_a *= inv;
      *sum_b *= inv;
    }
    (self.out_width, self.out_height, self.sum_actual, self.sum_expected)
  }
}

#[inline]
fn sat_sum(sat: &[f64], stride: usize, x0: u32, y0: u32, x1: u32, y1: u32) -> f64 {
  let (x0, y0, x1, y1) = (x0 as usize, y0 as usize, x1 as usize, y1 as usize);
  let a = y1 * stride + x1;
  let b = y0 * stride + x1;
  let c = y1 * stride + x0;
  let d = y0 * stride + x0;
  sat[a] - sat[b] - sat[c] + sat[d]
}

fn windowed_ssim_similarity(actual: &[f64], expected: &[f64], width: u32, height: u32) -> f64 {
  if width == 0 || height == 0 {
    return 1.0;
  }

  debug_assert_eq!(actual.len(), expected.len());
  debug_assert_eq!(actual.len(), (width as usize) * (height as usize));

  // If the downsampled image is too small for local SSIM windows, fall back to a global SSIM.
  if width < PERCEPTUAL_WINDOW_SIZE || height < PERCEPTUAL_WINDOW_SIZE {
    let mut acc = SsimAccumulator::default();
    for (a, b) in actual.iter().zip(expected.iter()) {
      acc.push(*a, *b);
    }
    return acc.finish().clamp(0.0, 1.0);
  }

  let stride = (width + 1) as usize;
  let sat_len = stride * (height as usize + 1);

  // Summed-area tables for X, Y, X², Y², and X·Y.
  let mut sat_x = vec![0.0f64; sat_len];
  let mut sat_y = vec![0.0f64; sat_len];
  let mut sat_x2 = vec![0.0f64; sat_len];
  let mut sat_y2 = vec![0.0f64; sat_len];
  let mut sat_xy = vec![0.0f64; sat_len];

  for y in 0..height {
    let mut row_sum_x = 0.0;
    let mut row_sum_y = 0.0;
    let mut row_sum_x2 = 0.0;
    let mut row_sum_y2 = 0.0;
    let mut row_sum_xy = 0.0;

    let row_base = (y as usize) * (width as usize);
    let sat_row = (y as usize + 1) * stride;
    let sat_prev = (y as usize) * stride;

    for x in 0..width {
      let idx = row_base + x as usize;
      let a = actual[idx];
      let b = expected[idx];

      row_sum_x += a;
      row_sum_y += b;
      row_sum_x2 += a * a;
      row_sum_y2 += b * b;
      row_sum_xy += a * b;

      let sat_idx = sat_row + x as usize + 1;
      let above = sat_prev + x as usize + 1;

      sat_x[sat_idx] = sat_x[above] + row_sum_x;
      sat_y[sat_idx] = sat_y[above] + row_sum_y;
      sat_x2[sat_idx] = sat_x2[above] + row_sum_x2;
      sat_y2[sat_idx] = sat_y2[above] + row_sum_y2;
      sat_xy[sat_idx] = sat_xy[above] + row_sum_xy;
    }
  }

  let c1 = (0.01f64 * 255.0f64).powi(2);
  let c2 = (0.03f64 * 255.0f64).powi(2);
  let win = PERCEPTUAL_WINDOW_SIZE;
  let n = (win * win) as f64;

  let mut ssim_sum = 0.0f64;
  let mut windows = 0u64;

  let max_x0 = (width - win) as usize;
  let max_y0 = (height - win) as usize;

  for y0 in 0..=max_y0 {
    let y0 = y0 as u32;
    let y1 = y0 + win;
    for x0 in 0..=max_x0 {
      let x0 = x0 as u32;
      let x1 = x0 + win;

      let sum_x = sat_sum(&sat_x, stride, x0, y0, x1, y1);
      let sum_y = sat_sum(&sat_y, stride, x0, y0, x1, y1);
      let sum_x2 = sat_sum(&sat_x2, stride, x0, y0, x1, y1);
      let sum_y2 = sat_sum(&sat_y2, stride, x0, y0, x1, y1);
      let sum_xy = sat_sum(&sat_xy, stride, x0, y0, x1, y1);

      let mu_x = sum_x / n;
      let mu_y = sum_y / n;

      let var_x = (sum_x2 / n - mu_x * mu_x).max(0.0);
      let var_y = (sum_y2 / n - mu_y * mu_y).max(0.0);
      let cov_xy = sum_xy / n - mu_x * mu_y;

      let numerator = (2.0 * mu_x * mu_y + c1) * (2.0 * cov_xy + c2);
      let denominator = (mu_x * mu_x + mu_y * mu_y + c1) * (var_x + var_y + c2);

      let ssim = if denominator.is_finite() && denominator != 0.0 {
        numerator / denominator
      } else {
        1.0
      };

      // Clamp per-window SSIM to [0, 1] before aggregation. This prevents the global score from
      // collapsing to 0 when only some windows are anti-correlated.
      ssim_sum += ssim.clamp(0.0, 1.0);
      windows += 1;
    }
  }

  if windows == 0 {
    1.0
  } else {
    (ssim_sum / windows as f64).clamp(0.0, 1.0)
  }
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

  fn pattern(width: u32, height: u32) -> RgbaImage {
    let mut img = RgbaImage::new(width, height);
    for y in 0..height {
      for x in 0..width {
        // A deterministic, moderately high-frequency pattern that exercises both luma mean and
        // variance.
        let r = ((x.wrapping_mul(13) + y.wrapping_mul(7)) % 256) as u8;
        let g = ((x.wrapping_mul(3) + y.wrapping_mul(17)) % 256) as u8;
        let b = ((x.wrapping_mul(11) + y.wrapping_mul(5)) % 256) as u8;
        img.put_pixel(x, y, Rgba([r, g, b, 255]));
      }
    }
    img
  }

  fn add_noise(mut img: RgbaImage, seed: u64, amplitude: i16) -> RgbaImage {
    // Simple deterministic LCG.
    let mut s = seed;
    for px in img.pixels_mut() {
      s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
      let n = ((s >> 32) as u32 % (u32::from(amplitude as u16) * 2 + 1)) as i16 - amplitude;
      for c in 0..3 {
        let v = px[c] as i16 + n;
        px[c] = v.clamp(0, 255) as u8;
      }
    }
    img
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
    assert_eq!(diff.statistics.first_mismatch, Some((0, 0)));
    assert_eq!(
      diff.statistics.first_mismatch_rgba,
      Some(([255, 0, 0, 255], [0, 0, 0, 255]))
    );
    assert!(diff.diff_image.is_some());
  }

  #[test]
  fn ignores_alpha_when_configured() {
    let img_a = solid([100, 100, 100, 255]);
    let img_b = solid([100, 100, 100, 10]);

    let strict = compare_images(&img_a, &img_b, &CompareConfig::strict());
    assert!(!strict.is_match());
    // Regression test: perceptual distance must be alpha-sensitive when `compare_alpha=true`.
    // The RGB channels are identical, so if alpha is accidentally ignored in the perceptual
    // metric this will drop to ~0.
    assert!(strict.statistics.perceptual_distance > 0.05);

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

  #[test]
  fn perceptual_distance_does_not_saturate_for_partial_inversion() {
    // Construct an image where the left half is identical and the right half is strongly
    // anti-correlated. A global SSIM computed over the whole image can yield <= 0 similarity and
    // saturate the distance at 1.0; a windowed SSIM should preserve the fact that half the image
    // matches exactly.
    let width = 64;
    let height = 64;
    let expected = pattern(width, height);
    let mut actual = expected.clone();

    // Invert right half.
    for y in 0..height {
      for x in (width / 2)..width {
        let p = expected.get_pixel(x, y);
        actual.put_pixel(x, y, Rgba([255 - p[0], 255 - p[1], 255 - p[2], 255]));
      }
    }

    let config = CompareConfig::strict().with_generate_diff_image(false);
    let diff = compare_images(&actual, &expected, &config);
    assert!(diff.statistics.perceptual_distance > 0.0);
    assert!(
      diff.statistics.perceptual_distance < 0.99,
      "distance unexpectedly saturated: {}",
      diff.statistics.perceptual_distance
    );
  }

  #[test]
  fn perceptual_distance_increases_with_noise_amplitude() {
    let base = pattern(64, 64);
    let small_noise = add_noise(base.clone(), 1, 3);
    let large_noise = add_noise(base.clone(), 1, 20);

    let config = CompareConfig::strict().with_generate_diff_image(false);
    let d_small = compare_images(&small_noise, &base, &config).statistics.perceptual_distance;
    let d_large = compare_images(&large_noise, &base, &config).statistics.perceptual_distance;

    assert!(
      d_small < d_large,
      "expected small noise distance < large noise distance, got {d_small} vs {d_large}"
    );
  }
}
