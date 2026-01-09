use crate::error::Error;
use crate::error::RenderError;
use crate::error::Result;
use crate::fallible_vec_writer::FallibleVecWriter;
use crate::image_compare::{self, CompareConfig};
use crate::paint::pixmap::reserve_buffer;
use crate::paint::pixmap::MAX_PIXMAP_BYTES;
use image::GenericImageView;
use image::Rgba;
use image::RgbaImage;
use image::Rgb;
use std::ffi::c_void;
use std::io::Write;
use tiny_skia::Pixmap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RgbaAlphaMode {
  /// The RGBA output buffer uses premultiplied alpha, matching tiny-skia's internal `Pixmap`
  /// representation.
  Premultiplied,
  /// The RGBA output buffer uses straight/unpremultiplied alpha, matching the semantics used by
  /// [`encode_image`] for PNG/JPEG/WebP output.
  Straight,
}

/// Summary of a pixel diff operation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiffMetrics {
  pub pixel_diff: u64,
  pub total_pixels: u64,
  pub diff_percentage: f64,
  /// SSIM-derived perceptual distance (0.0 = identical, higher = more different).
  pub perceptual_distance: f64,
  /// Maximum per-channel delta across all compared pixels (0-255).
  ///
  /// When `compare_alpha` is false, alpha deltas are ignored for this metric.
  pub max_channel_diff: u8,
  /// Dimensions of the rendered/actual image.
  pub rendered_dimensions: (u32, u32),
  /// Dimensions of the expected/baseline image.
  pub expected_dimensions: (u32, u32),
  /// Location (x,y) of the first pixel that exceeded the configured tolerance.
  ///
  /// The scan order is row-major (left-to-right, top-to-bottom).
  pub first_mismatch: Option<(u32, u32)>,
  /// RGBA samples at [`Self::first_mismatch`], stored as `(rendered, expected)`.
  ///
  /// When dimensions differ, pixels missing from one side are treated as transparent black
  /// (`[0, 0, 0, 0]`).
  pub first_mismatch_rgba: Option<([u8; 4], [u8; 4])>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OutputFormat {
  Png,
  Jpeg(u8), // quality 0-100
  WebP(u8), // quality 0-100
}

impl Default for OutputFormat {
  fn default() -> Self {
    OutputFormat::Png
  }
}

#[inline]
fn unpremultiply_rgb(r: u8, g: u8, b: u8, a: u8) -> (u8, u8, u8) {
  if a == 0 {
    return (0, 0, 0);
  }

  // Match the legacy unpremultiplication semantics exactly:
  // - compute alpha as f32
  // - divide each channel by alpha
  // - clamp to 255 and truncate toward zero.
  let alpha = a as f32 / 255.0;
  (
    ((r as f32 / alpha).min(255.0)) as u8,
    ((g as f32 / alpha).min(255.0)) as u8,
    ((b as f32 / alpha).min(255.0)) as u8,
  )
}

#[inline]
fn unpremultiply_rgba_row(src: &[u8], dst: &mut [u8]) {
  debug_assert_eq!(src.len(), dst.len());
  debug_assert_eq!(src.len() % 4, 0);

  for (in_px, out_px) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
    let (r, g, b) = unpremultiply_rgb(in_px[0], in_px[1], in_px[2], in_px[3]);
    out_px[0] = r;
    out_px[1] = g;
    out_px[2] = b;
    out_px[3] = in_px[3];
  }
}

fn checked_pixmap_len(width: u32, height: u32, context: &str) -> Result<usize> {
  // Guard against absurdly large pixmaps: even though the pixmap already exists, consumers may
  // allocate temporary buffers (or in the case of `pixmap_to_rgba8`, allocate a second full-frame
  // RGBA buffer) and we want to fail gracefully instead of risking an abort on OOM.
  if width == 0 || height == 0 {
    return Err(Error::Render(RenderError::InvalidParameters {
      message: format!("{context}: pixmap size is zero ({width}x{height})"),
    }));
  }

  let expected_len = u64::from(width)
    .checked_mul(u64::from(height))
    .and_then(|px| px.checked_mul(4))
    .ok_or_else(|| {
      Error::Render(RenderError::InvalidParameters {
        message: format!("{context}: pixmap dimensions overflow ({width}x{height})"),
      })
    })?;
  if expected_len > MAX_PIXMAP_BYTES {
    return Err(Error::Render(RenderError::InvalidParameters {
      message: format!(
        "{context}: pixmap {}x{} is {} bytes (limit {})",
        width, height, expected_len, MAX_PIXMAP_BYTES
      ),
    }));
  }

  usize::try_from(expected_len).map_err(|_| {
    Error::Render(RenderError::InvalidParameters {
      message: format!("{context}: pixmap byte size does not fit in usize ({width}x{height})"),
    })
  })
}

fn checked_pixmap_data<'a>(
  pixmap: &'a Pixmap,
  context: &str,
) -> Result<(u32, u32, &'a [u8], usize)> {
  let width = pixmap.width();
  let height = pixmap.height();
  let pixels = pixmap.data();

  let expected_len_usize = checked_pixmap_len(width, height, context)?;
  if pixels.len() != expected_len_usize {
    return Err(Error::Render(RenderError::InvalidParameters {
      message: format!(
        "{context}: pixmap data length mismatch (expected {expected_len_usize} bytes, got {})",
        pixels.len()
      ),
    }));
  }

  Ok((width, height, pixels, expected_len_usize))
}

pub fn pixmap_to_rgba8(pixmap: &Pixmap, mode: RgbaAlphaMode) -> Result<Vec<u8>> {
  let (width, _height, pixels, expected_len) = checked_pixmap_data(pixmap, "pixmap_to_rgba8")?;
  let expected_len_u64 = u64::try_from(expected_len).map_err(|_| {
    Error::Render(RenderError::InvalidParameters {
      message: format!("pixmap_to_rgba8: pixmap byte size does not fit in u64 ({width}px wide)"),
    })
  })?;

  match mode {
    RgbaAlphaMode::Premultiplied => {
      let mut out =
        reserve_buffer(expected_len_u64, "pixmap_to_rgba8: RGBA output buffer").map_err(Error::Render)?;
      out.extend_from_slice(pixels);
      Ok(out)
    }
    RgbaAlphaMode::Straight => {
      let row_len = (width as usize).checked_mul(4).ok_or_else(|| {
        Error::Render(RenderError::InvalidParameters {
          message: format!("pixmap_to_rgba8: row byte size overflow (width={width})"),
        })
      })?;

      let mut out =
        reserve_buffer(expected_len_u64, "pixmap_to_rgba8: RGBA output buffer").map_err(Error::Render)?;
      out.resize(expected_len, 0);

      for (src_row, dst_row) in pixels
        .chunks_exact(row_len)
        .zip(out.chunks_exact_mut(row_len))
      {
        unpremultiply_rgba_row(src_row, dst_row);
      }

      Ok(out)
    }
  }
}

pub fn encode_image(pixmap: &Pixmap, format: OutputFormat) -> Result<Vec<u8>> {
  let (width, height, pixels, _expected_len) = checked_pixmap_data(pixmap, "encode_image")?;

  match format {
    OutputFormat::Png => {
      // Stream rows to the encoder so we don't allocate a full-frame straight-RGBA buffer.
      let row_len = (width as usize).checked_mul(4).ok_or_else(|| {
        Error::Render(RenderError::InvalidParameters {
          message: format!("encode_image: row byte size overflow (width={width})"),
        })
      })?;

      // Cap encoded output to the same budget we use for pixmap allocations so huge outputs fail
      // with a structured error instead of aborting the process.
      let mut buffer = FallibleVecWriter::new(MAX_PIXMAP_BYTES as usize, "encode_image: PNG output");
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

        let row_len_u64 = u64::try_from(row_len).map_err(|_| {
          Error::Render(RenderError::InvalidParameters {
            message: "encode_image: PNG row buffer length does not fit in u64".to_string(),
          })
        })?;
        let mut row_buf = reserve_buffer(row_len_u64, "encode_image: PNG row buffer")
          .map_err(Error::Render)?;
        row_buf.resize(row_len, 0);
        for row in pixels.chunks_exact(row_len) {
          unpremultiply_rgba_row(row, &mut row_buf);
          stream.write_all(&row_buf).map_err(|e| {
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
    OutputFormat::Jpeg(quality) => {
      let quality = quality.min(100);
      // JPEG has no alpha channel. Use the `image` crate's streaming-friendly `encode_image`
      // (which iterates over pixels on demand) so we don't allocate a full-frame RGB buffer.
      struct UnpremultipliedRgbView<'a> {
        width: u32,
        height: u32,
        pixels: &'a [u8],
      }

      impl GenericImageView for UnpremultipliedRgbView<'_> {
        type Pixel = Rgb<u8>;

        fn dimensions(&self) -> (u32, u32) {
          (self.width, self.height)
        }

        fn get_pixel(&self, x: u32, y: u32) -> Self::Pixel {
          if x >= self.width || y >= self.height {
            // Defensive fallback: the `image` crate's encoders should never request out-of-bounds
            // pixels when the `dimensions()` implementation is correct, but avoid panicking in
            // production if a caller does.
            return Rgb([0, 0, 0]);
          }

          let idx = (y as usize * self.width as usize + x as usize) * 4;
          let in_px = &self.pixels[idx..idx + 4];
          let (r, g, b) = unpremultiply_rgb(in_px[0], in_px[1], in_px[2], in_px[3]);
          Rgb([r, g, b])
        }
      }

      let view = UnpremultipliedRgbView {
        width,
        height,
        pixels,
      };

      let mut buffer = FallibleVecWriter::new(MAX_PIXMAP_BYTES as usize, "encode_image: JPEG output");
      let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buffer, quality);
      encoder.encode_image(&view).map_err(|e| {
        Error::Render(RenderError::EncodeFailed {
          format: "JPEG".to_string(),
          reason: e.to_string(),
        })
      })?;

      Ok(buffer.into_inner())
    }
    OutputFormat::WebP(quality) => {
      // Encode via libwebp's picture API to avoid building a second full-frame RGBA buffer.
      let width_i32 = i32::try_from(width).map_err(|_| {
        Error::Render(RenderError::InvalidParameters {
          message: format!("encode_image: WebP width out of range ({width})"),
        })
      })?;
      let height_i32 = i32::try_from(height).map_err(|_| {
        Error::Render(RenderError::InvalidParameters {
          message: format!("encode_image: WebP height out of range ({height})"),
        })
      })?;

      let quality = (quality as f32).clamp(0.0, 100.0);

      unsafe {
        let mut config: libwebp_sys::WebPConfig = std::mem::zeroed();
        if libwebp_sys::WebPConfigInitInternal(
          &mut config,
          libwebp_sys::WebPPreset::WEBP_PRESET_DEFAULT,
          quality,
          libwebp_sys::WEBP_ENCODER_ABI_VERSION as i32,
        ) == 0
        {
          return Err(Error::Render(RenderError::EncodeFailed {
            format: "WebP".to_string(),
            reason: "WebPConfigInitInternal failed".to_string(),
          }));
        }
        config.lossless = 0;

        if libwebp_sys::WebPValidateConfig(&config) == 0 {
          return Err(Error::Render(RenderError::EncodeFailed {
            format: "WebP".to_string(),
            reason: "WebPValidateConfig failed".to_string(),
          }));
        }

        let mut picture: libwebp_sys::WebPPicture = std::mem::zeroed();
        if libwebp_sys::WebPPictureInitInternal(
          &mut picture,
          libwebp_sys::WEBP_ENCODER_ABI_VERSION as i32,
        ) == 0
        {
          return Err(Error::Render(RenderError::EncodeFailed {
            format: "WebP".to_string(),
            reason: "WebPPictureInitInternal failed".to_string(),
          }));
        }

        struct PictureGuard(libwebp_sys::WebPPicture);
        impl Drop for PictureGuard {
          fn drop(&mut self) {
            unsafe {
              libwebp_sys::WebPPictureFree(&mut self.0);
            }
          }
        }
        let mut picture = PictureGuard(picture);

        picture.0.width = width_i32;
        picture.0.height = height_i32;
        picture.0.use_argb = 1;
        if libwebp_sys::WebPPictureAlloc(&mut picture.0) == 0 {
          return Err(Error::Render(RenderError::EncodeFailed {
            format: "WebP".to_string(),
            reason: "WebPPictureAlloc failed".to_string(),
          }));
        }

        let stride = usize::try_from(picture.0.argb_stride).map_err(|_| {
          Error::Render(RenderError::EncodeFailed {
            format: "WebP".to_string(),
            reason: "WebP argb_stride out of range".to_string(),
          })
        })?;
        let pixels_per_row = width as usize;
        if stride < pixels_per_row {
          return Err(Error::Render(RenderError::EncodeFailed {
            format: "WebP".to_string(),
            reason: format!(
              "WebP argb_stride ({}) smaller than width ({pixels_per_row})",
              picture.0.argb_stride
            ),
          }));
        }

        let argb_len = stride
          .checked_mul(height as usize)
          .ok_or_else(|| {
            Error::Render(RenderError::InvalidParameters {
              message: "encode_image: WebP ARGB buffer length overflow".to_string(),
            })
          })?;
        let argb = std::slice::from_raw_parts_mut(picture.0.argb, argb_len);

        for (y, row) in pixels.chunks_exact(pixels_per_row * 4).enumerate() {
          let dst_row = &mut argb[y * stride..y * stride + pixels_per_row];
          for (in_px, out_px) in row.chunks_exact(4).zip(dst_row.iter_mut()) {
            let (r, g, b) = unpremultiply_rgb(in_px[0], in_px[1], in_px[2], in_px[3]);
            let a = in_px[3];
            // libwebp expects ARGB packed as 0xAARRGGBB.
            *out_px =
              (u32::from(a) << 24) | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
          }
        }

        let mut writer: libwebp_sys::WebPMemoryWriter = std::mem::zeroed();
        libwebp_sys::WebPMemoryWriterInit(&mut writer);

        struct WriterGuard(libwebp_sys::WebPMemoryWriter);
        impl Drop for WriterGuard {
          fn drop(&mut self) {
            unsafe {
              libwebp_sys::WebPMemoryWriterClear(&mut self.0);
            }
          }
        }
        let mut writer = WriterGuard(writer);

        picture.0.writer = Some(libwebp_sys::WebPMemoryWrite);
        picture.0.custom_ptr = (&mut writer.0 as *mut libwebp_sys::WebPMemoryWriter).cast::<c_void>();

        if libwebp_sys::WebPEncode(&config, &mut picture.0) == 0 {
          return Err(Error::Render(RenderError::EncodeFailed {
            format: "WebP".to_string(),
            reason: format!("WebP encode failed: {:?}", picture.0.error_code),
          }));
        }

        let data = std::slice::from_raw_parts(writer.0.mem, writer.0.size);
        let out_len = u64::try_from(writer.0.size).map_err(|_| {
          Error::Render(RenderError::EncodeFailed {
            format: "WebP".to_string(),
            reason: "WebP output size does not fit in u64".to_string(),
          })
        })?;
        let mut out = reserve_buffer(out_len, "encode_image: WebP output").map_err(Error::Render)?;
        out.extend_from_slice(data);
        Ok(out)
      }
    }
  }
}

/// Computes a diff image between two PNG byte buffers.
///
/// Returns the diff metrics along with a PNG highlighting differing pixels.
pub fn diff_png(rendered: &[u8], expected: &[u8], tolerance: u8) -> Result<(DiffMetrics, Vec<u8>)> {
  diff_png_with_alpha(rendered, expected, tolerance, true)
}

/// Like [`diff_png`], but allows controlling whether alpha differences are considered.
pub fn diff_png_with_alpha(
  rendered: &[u8],
  expected: &[u8],
  tolerance: u8,
  compare_alpha: bool,
) -> Result<(DiffMetrics, Vec<u8>)> {
  let mut config = CompareConfig::strict()
    .with_channel_tolerance(tolerance)
    .with_compare_alpha(compare_alpha);
  config.max_different_percent = 100.0;

  let diff = image_compare::compare_png(rendered, expected, &config)?;
  if diff.dimensions_match {
    let diff_png = diff.diff_png()?.ok_or_else(|| {
      Error::Render(RenderError::EncodeFailed {
        format: "PNG".to_string(),
        reason: "diff image was not generated".to_string(),
      })
    })?;

    let metrics = DiffMetrics {
      pixel_diff: diff.statistics.different_pixels,
      total_pixels: diff.statistics.total_pixels,
      diff_percentage: diff.statistics.different_percent,
      perceptual_distance: diff.statistics.perceptual_distance,
      max_channel_diff: diff.statistics.max_channel_diff(compare_alpha),
      rendered_dimensions: diff.actual_dimensions,
      expected_dimensions: diff.expected_dimensions,
      first_mismatch: diff.statistics.first_mismatch,
      first_mismatch_rgba: diff.statistics.first_mismatch_rgba,
    };

    return Ok((metrics, diff_png));
  }

  // When dimensions differ, fall back to a padded diff so reports remain usable (mirrors the old
  // `cargo xtask diff-renders` behaviour). Missing pixels are treated as differences.
  let rendered_img = image_compare::decode_png(rendered)?;
  let expected_img = image_compare::decode_png(expected)?;

  let max_width = rendered_img.width().max(expected_img.width());
  let max_height = rendered_img.height().max(expected_img.height());
  // Total pixels should count the union of both image rectangles. When the max width comes from
  // one image and the max height comes from the other, the bottom-right corner can be missing
  // from both. Counting only pixels that are present in at least one image avoids panics and
  // produces a more meaningful diff percentage.
  let mut total_pixels = 0u64;

  let diff_bytes = u64::from(max_width)
    .checked_mul(u64::from(max_height))
    .and_then(|px| px.checked_mul(4))
    .ok_or_else(|| {
      Error::Render(RenderError::InvalidParameters {
        message: format!("diff_png_with_alpha: diff image dimensions overflow ({max_width}x{max_height})"),
      })
    })?;
  if diff_bytes > MAX_PIXMAP_BYTES {
    return Err(Error::Render(RenderError::InvalidParameters {
      message: format!(
        "diff_png_with_alpha: diff image {}x{} is {} bytes (limit {})",
        max_width, max_height, diff_bytes, MAX_PIXMAP_BYTES
      ),
    }));
  }

  let diff_len = usize::try_from(diff_bytes).map_err(|_| {
    Error::Render(RenderError::InvalidParameters {
      message: format!("diff_png_with_alpha: diff image byte size does not fit in usize ({max_width}x{max_height})"),
    })
  })?;
  let mut diff_buf = reserve_buffer(diff_bytes, "diff_png_with_alpha: diff image buffer")
    .map_err(Error::Render)?;
  diff_buf.resize(diff_len, 0);
  let mut diff_image = RgbaImage::from_raw(max_width, max_height, diff_buf).ok_or_else(|| {
    Error::Render(RenderError::InvalidParameters {
      message: "diff_png_with_alpha: invalid diff image buffer".to_string(),
    })
  })?;
  let mut different_pixels = 0u64;
  let mut max_channel_diff = 0u8;
  let mut first_mismatch: Option<(u32, u32)> = None;
  let mut first_mismatch_rgba: Option<([u8; 4], [u8; 4])> = None;

  for y in 0..max_height {
    for x in 0..max_width {
      let rendered_px = if x < rendered_img.width() && y < rendered_img.height() {
        Some(*rendered_img.get_pixel(x, y))
      } else {
        None
      };
      let expected_px = if x < expected_img.width() && y < expected_img.height() {
        Some(*expected_img.get_pixel(x, y))
      } else {
        None
      };

      match (rendered_px, expected_px) {
        (Some(rendered_px), Some(expected_px)) => {
          total_pixels += 1;
          let diff_r = rendered_px[0].abs_diff(expected_px[0]);
          let diff_g = rendered_px[1].abs_diff(expected_px[1]);
          let diff_b = rendered_px[2].abs_diff(expected_px[2]);
          let diff_a = if compare_alpha {
            rendered_px[3].abs_diff(expected_px[3])
          } else {
            0
          };
          let max_diff = diff_r.max(diff_g).max(diff_b).max(diff_a);
          max_channel_diff = max_channel_diff.max(max_diff);

          let is_different = if compare_alpha {
            diff_r > tolerance || diff_g > tolerance || diff_b > tolerance || diff_a > tolerance
          } else {
            diff_r > tolerance || diff_g > tolerance || diff_b > tolerance
          };
          if is_different {
            different_pixels += 1;
            if first_mismatch.is_none() {
              first_mismatch = Some((x, y));
              first_mismatch_rgba = Some((
                [rendered_px[0], rendered_px[1], rendered_px[2], rendered_px[3]],
                [expected_px[0], expected_px[1], expected_px[2], expected_px[3]],
              ));
            }
            let alpha = max_diff.saturating_mul(2).min(255);
            diff_image.put_pixel(x, y, Rgba([255, 0, 0, alpha]));
          } else {
            diff_image.put_pixel(x, y, Rgba([0, 0, 0, 0]));
          }
        }
        (Some(rendered_px), None) => {
          total_pixels += 1;
          different_pixels += 1;
          max_channel_diff = 255;
          if first_mismatch.is_none() {
            first_mismatch = Some((x, y));
            first_mismatch_rgba = Some((
              [rendered_px[0], rendered_px[1], rendered_px[2], rendered_px[3]],
              [0, 0, 0, 0],
            ));
          }
          diff_image.put_pixel(x, y, Rgba([255, 0, 255, 255]));
        }
        (None, Some(expected_px)) => {
          total_pixels += 1;
          different_pixels += 1;
          max_channel_diff = 255;
          if first_mismatch.is_none() {
            first_mismatch = Some((x, y));
            first_mismatch_rgba = Some((
              [0, 0, 0, 0],
              [expected_px[0], expected_px[1], expected_px[2], expected_px[3]],
            ));
          }
          diff_image.put_pixel(x, y, Rgba([255, 0, 255, 255]));
        }
        (None, None) => {
          // Both images are missing this pixel (can happen when each image provides one of the max
          // dimensions). Treat this as a transparent match.
          diff_image.put_pixel(x, y, Rgba([0, 0, 0, 0]));
        }
      }
    }
  }

  let diff_percentage = if total_pixels > 0 {
    (different_pixels as f64 / total_pixels as f64) * 100.0
  } else {
    0.0
  };

  let diff_png = image_compare::encode_png(&diff_image)?;
  let metrics = DiffMetrics {
    pixel_diff: different_pixels,
    total_pixels,
    diff_percentage,
    // Perceptual distance is ill-defined when dimensions don't match; treat this as maximally
    // different for now.
    perceptual_distance: 1.0,
    max_channel_diff,
    rendered_dimensions: (rendered_img.width(), rendered_img.height()),
    expected_dimensions: (expected_img.width(), expected_img.height()),
    first_mismatch,
    first_mismatch_rgba,
  };

  Ok((metrics, diff_png))
}

#[cfg(test)]
mod tests {
  use super::*;
  use image::{Rgba, RgbaImage};

  #[test]
  fn pixmap_to_rgba8_converts_premultiplied_and_straight() {
    let mut pixmap = Pixmap::new(3, 1).expect("pixmap");
    let premultiplied: [u8; 12] = [
      5, 10, 15, 0, // alpha=0 -> RGB should become 0 in straight mode
      64, 32, 128, 128, // alpha=128 -> exercises f32 rounding
      200, 100, 50, 255, // alpha=255 -> unchanged
    ];
    pixmap.data_mut().copy_from_slice(&premultiplied);

    let premultiplied_out =
      pixmap_to_rgba8(&pixmap, RgbaAlphaMode::Premultiplied).expect("premultiplied");
    assert_eq!(&premultiplied_out, premultiplied.as_slice());

    let straight_out = pixmap_to_rgba8(&pixmap, RgbaAlphaMode::Straight).expect("straight");
    let expected_straight: [u8; 12] = [0, 0, 0, 0, 127, 63, 254, 128, 200, 100, 50, 255];
    assert_eq!(&straight_out, expected_straight.as_slice());
  }

  #[test]
  fn pixmap_len_rejects_oversized_dimensions() {
    let width = (MAX_PIXMAP_BYTES / 4 + 1) as u32;
    let height = 1;
    assert!(matches!(
      checked_pixmap_len(width, height, "pixmap_to_rgba8"),
      Err(Error::Render(RenderError::InvalidParameters { .. }))
    ));
  }

  #[test]
  fn diff_png_with_alpha_records_first_mismatch() {
    let mut rendered = RgbaImage::from_pixel(2, 2, Rgba([0, 0, 0, 255]));
    let expected = RgbaImage::from_pixel(2, 2, Rgba([0, 0, 0, 255]));
    rendered.put_pixel(1, 0, Rgba([255, 0, 0, 255]));

    let rendered_png = crate::image_compare::encode_png(&rendered).expect("encode rendered");
    let expected_png = crate::image_compare::encode_png(&expected).expect("encode expected");

    let (metrics, _diff_png) =
      diff_png_with_alpha(&rendered_png, &expected_png, 0, true).expect("diff");
    assert_eq!(metrics.first_mismatch, Some((1, 0)));
    assert_eq!(
      metrics.first_mismatch_rgba,
      Some(([255, 0, 0, 255], [0, 0, 0, 255]))
    );
  }

  #[test]
  fn diff_png_with_alpha_records_first_mismatch_for_dimension_mismatch() {
    let rendered = RgbaImage::from_pixel(2, 1, Rgba([10, 20, 30, 255]));
    let expected = RgbaImage::from_pixel(1, 1, Rgba([10, 20, 30, 255]));

    let rendered_png = crate::image_compare::encode_png(&rendered).expect("encode rendered");
    let expected_png = crate::image_compare::encode_png(&expected).expect("encode expected");

    let (metrics, _diff_png) =
      diff_png_with_alpha(&rendered_png, &expected_png, 0, true).expect("diff");
    assert_eq!(metrics.rendered_dimensions, (2, 1));
    assert_eq!(metrics.expected_dimensions, (1, 1));
    assert_eq!(metrics.first_mismatch, Some((1, 0)));
    assert_eq!(
      metrics.first_mismatch_rgba,
      Some(([10, 20, 30, 255], [0, 0, 0, 0]))
    );
  }

  #[test]
  fn diff_png_with_alpha_handles_disjoint_corner_for_dimension_mismatch() {
    // rendered contributes max height, expected contributes max width => bottom-right corner is
    // out-of-bounds for both images.
    let rendered = RgbaImage::from_pixel(1, 2, Rgba([10, 20, 30, 255]));
    let expected = RgbaImage::from_pixel(2, 1, Rgba([10, 20, 30, 255]));

    let rendered_png = crate::image_compare::encode_png(&rendered).expect("encode rendered");
    let expected_png = crate::image_compare::encode_png(&expected).expect("encode expected");

    let (metrics, _diff_png) =
      diff_png_with_alpha(&rendered_png, &expected_png, 0, true).expect("diff");
    assert_eq!(metrics.rendered_dimensions, (1, 2));
    assert_eq!(metrics.expected_dimensions, (2, 1));
    assert_eq!(metrics.total_pixels, 3, "union of 1x2 and 2x1 should be 3 pixels");
    assert_eq!(metrics.pixel_diff, 2);
    assert_eq!(metrics.first_mismatch, Some((1, 0)));
    assert_eq!(
      metrics.first_mismatch_rgba,
      Some(([0, 0, 0, 0], [10, 20, 30, 255]))
    );
    let expected_diff_percent = (2.0 / 3.0) * 100.0;
    assert!(
      (metrics.diff_percentage - expected_diff_percent).abs() < 0.01,
      "expected diff_percent {:.4}, got {:.4}",
      expected_diff_percent,
      metrics.diff_percentage
    );
  }
}
