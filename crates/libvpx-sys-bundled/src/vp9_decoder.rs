use std::ffi::CStr;
use std::fmt;
use std::ptr;

/// Hard maximum width/height accepted from untrusted VP9 bitstreams.
///
/// This is a defense-in-depth cap to prevent adversarial/corrupted media from causing unbounded
/// allocations.
const MAX_VIDEO_DIMENSION: u32 = 8192;

/// Hard maximum bytes accepted for a single decoded RGBA8 frame.
const MAX_VIDEO_FRAME_BYTES: usize = 128 * 1024 * 1024;

/// Errors emitted by media decoders.
///
/// This type intentionally lives in the codec layer (as opposed to using `std::io::Error`) so we
/// can report clear "unsupported" messages for files we can parse but choose not to handle (e.g.
/// 10/12-bit VP9 output frames).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaError {
  Unsupported(String),
  Decode(String),
}

impl fmt::Display for MediaError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Unsupported(msg) => write!(f, "unsupported: {msg}"),
      Self::Decode(msg) => write!(f, "decode error: {msg}"),
    }
  }
}

impl std::error::Error for MediaError {}

/// An RGBA8 VP9 video frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vp9Frame {
  pub width: u32,
  pub height: u32,
  /// Intended rendering width, if the stream specifies non-square pixel aspect ratio metadata.
  ///
  /// This corresponds to `vpx_image_t.r_w` and may differ from `width`.
  pub render_width: u32,
  /// Intended rendering height, if the stream specifies non-square pixel aspect ratio metadata.
  ///
  /// This corresponds to `vpx_image_t.r_h` and may differ from `height`.
  pub render_height: u32,
  pub rgba8: Vec<u8>,
}

/// VP9 bitstream decoder backed by libvpx.
///
/// This wrapper is intentionally minimal. It is **not** (yet) integrated into FastRender's media
/// pipeline; it exists as a standalone decoder utility that higher-level container demuxers can
/// call into.
pub struct Vp9Decoder {
  ctx: crate::vpx_codec_ctx_t,
  initialized: bool,
}

impl Vp9Decoder {
  /// `VPX_IMG_FMT_HIGHBITDEPTH` from `vpx/vpx_image.h`.
  const VPX_IMG_FMT_HIGHBITDEPTH: u32 = 0x800;
  /// `VPX_IMG_FMT_HAS_ALPHA` from `vpx/vpx_image.h`.
  const VPX_IMG_FMT_HAS_ALPHA: u32 = 0x400;

  /// Create a new VP9 decoder instance.
  pub fn new(threads: u32) -> Result<Self, MediaError> {
    let mut ctx: crate::vpx_codec_ctx_t = unsafe { std::mem::zeroed() };
    let iface = unsafe { crate::vpx_codec_vp9_dx() };
    if iface.is_null() {
      return Err(MediaError::Decode(
        "libvpx returned a null VP9 decoder interface".to_string(),
      ));
    }

    let cfg = crate::vpx_codec_dec_cfg_t {
      threads: threads.max(1),
      w: 0,
      h: 0,
    };

    let res = unsafe { crate::vpx_codec_dec_init(&mut ctx, iface, &cfg, 0) };
    if res != crate::VPX_CODEC_OK {
      let msg = codec_error_string(&mut ctx, Some(res));
      // Even on init failure, vpx_codec_destroy is safe to call (it will no-op if the ctx isn't
      // initialized); still, keep things explicit.
      unsafe {
        let _ = crate::vpx_codec_destroy(&mut ctx);
      }
      return Err(MediaError::Decode(format!("vp9 init failed: {msg}")));
    }

    Ok(Self {
      ctx,
      initialized: true,
    })
  }

  /// Decode a VP9 frame.
  ///
  /// Note: libvpx may output 0+ frames per input packet; the returned vec contains all frames
  /// produced by this call.
  ///
  /// If `data` is empty, this performs a libvpx **flush** (equivalent to calling
  /// `vpx_codec_decode(ctx, NULL, 0, ...)` in C) and returns any delayed frames.
  pub fn decode(&mut self, data: &[u8]) -> Result<Vec<Vp9Frame>, MediaError> {
    if !self.initialized {
      return Err(MediaError::Decode(
        "vp9 decoder not initialized".to_string(),
      ));
    }

    let (data_ptr, data_sz) = if data.is_empty() {
      (ptr::null(), 0u32)
    } else {
      (
        data.as_ptr(),
        data
          .len()
          .try_into()
          .map_err(|_| MediaError::Decode("vp9 frame too large".to_string()))?,
      )
    };
    let res = unsafe {
      crate::vpx_codec_decode(
        &mut self.ctx,
        data_ptr,
        data_sz,
        ptr::null_mut(),
        0,
      )
    };
    if res != crate::VPX_CODEC_OK {
      let msg = codec_error_string(&mut self.ctx, Some(res));
      return Err(MediaError::Decode(format!("vp9 decode failed: {msg}")));
    }

    let mut frames = Vec::new();
    let mut iter: crate::vpx_codec_iter_t = ptr::null();
    loop {
      let img_ptr = unsafe { crate::vpx_codec_get_frame(&mut self.ctx, &mut iter) };
      if img_ptr.is_null() {
        break;
      }
      let img = unsafe { &*img_ptr };
      frames.push(Self::rgba_from_image(img)?);
    }
    Ok(frames)
  }

  /// Convert a decoded `vpx_image_t` into an RGBA8 frame.
  ///
  /// This is where we must be careful about VP9 high bit depth output: libvpx will surface 10/12-bit
  /// content as 16-bit planes with `bit_depth` set to 10 or 12, and `fmt` tagged with
  /// `VPX_IMG_FMT_HIGHBITDEPTH`.
  ///
  /// For high bit depth output, we downshift the 16-bit YUV planes to 8-bit and then convert to
  /// RGBA8. This is lossy but avoids silently treating 16-bit planes as 8-bit.
  pub fn rgba_from_image(img: &crate::vpx_image_t) -> Result<Vp9Frame, MediaError> {
    let fmt = img.fmt as u32;
    let high_bit_depth = (fmt & Self::VPX_IMG_FMT_HIGHBITDEPTH) != 0;
    let has_alpha = (fmt & Self::VPX_IMG_FMT_HAS_ALPHA) != 0;
    // Strip flags so we can match on the underlying subsampling format.
    let fmt_base = fmt & !(Self::VPX_IMG_FMT_HIGHBITDEPTH | Self::VPX_IMG_FMT_HAS_ALPHA);
    let bit_depth = img.bit_depth;

    match fmt_base {
      crate::VPX_IMG_FMT_I420
      | crate::VPX_IMG_FMT_I422
      | crate::VPX_IMG_FMT_I444
      | crate::VPX_IMG_FMT_I440
      | crate::VPX_IMG_FMT_YV12 => {}
      _ => {
        return Err(MediaError::Unsupported(format!(
          "vp9 pixel format unsupported: fmt=0x{fmt:x}"
        )));
      }
    }

    // libvpx defines a 4:4:4+alpha format (VPX_IMG_FMT_444A). Support alpha only for 4:4:4 since
    // other subsampling + alpha formats are not part of the public API.
    if has_alpha && fmt_base != crate::VPX_IMG_FMT_I444 {
      return Err(MediaError::Unsupported(format!(
        "vp9 alpha format unsupported: fmt=0x{fmt:x}"
      )));
    }

    // Reject inconsistent metadata early: if the frame claims a non-8-bit bit depth but does not
    // set the high-bit-depth format flag, we cannot safely interpret the plane data.
    if !high_bit_depth && bit_depth != 8 {
      return Err(MediaError::Unsupported(format!(
        "vp9 bit_depth unsupported: bit_depth={bit_depth} fmt=0x{fmt:x}"
      )));
    }

    let width: usize = img
      .d_w
      .try_into()
      .map_err(|_| MediaError::Decode("vp9 frame width overflow".to_string()))?;
    let height: usize = img
      .d_h
      .try_into()
      .map_err(|_| MediaError::Decode("vp9 frame height overflow".to_string()))?;
    if width == 0 || height == 0 {
      return Err(MediaError::Decode(format!(
        "vp9 frame has invalid dimensions: {width}x{height}"
      )));
    }

    // Treat decoded dimensions as untrusted: reject absurdly large frames before touching any plane
    // pointers or allocating output buffers.
    let max_dim = MAX_VIDEO_DIMENSION as usize;
    if width > max_dim || height > max_dim {
      return Err(MediaError::Unsupported(format!(
        "vp9 frame dimensions {width}x{height} exceed hard cap {}x{}",
        MAX_VIDEO_DIMENSION, MAX_VIDEO_DIMENSION
      )));
    }

    let rgba_len = width
      .checked_mul(height)
      .and_then(|v| v.checked_mul(4))
      .ok_or_else(|| MediaError::Decode("vp9 frame buffer size overflow".to_string()))?;
    if rgba_len > MAX_VIDEO_FRAME_BYTES {
      return Err(MediaError::Unsupported(format!(
        "vp9 frame size {width}x{height} ({rgba_len} bytes) exceeds hard cap ({MAX_VIDEO_FRAME_BYTES} bytes)"
      )));
    }

    let y_plane = img.planes[0];
    // `VPX_IMG_FMT_YV12` is YVU (V and U swapped compared to I420). libvpx uses the format tag to
    // signal this plane ordering.
    let (u_plane, v_plane) = if fmt_base == crate::VPX_IMG_FMT_YV12 {
      (img.planes[2], img.planes[1])
    } else {
      (img.planes[1], img.planes[2])
    };
    let a_plane = img.planes[3];
    if y_plane.is_null() || u_plane.is_null() || v_plane.is_null() {
      return Err(MediaError::Decode(
        "vp9 frame has null Y/U/V plane pointers".to_string(),
      ));
    }
    if has_alpha && a_plane.is_null() {
      return Err(MediaError::Decode(
        "vp9 frame has null alpha plane pointer".to_string(),
      ));
    }

    let y_stride: usize = img.stride[0]
      .try_into()
      .map_err(|_| MediaError::Decode("vp9 Y stride negative".to_string()))?;
    let u_stride: usize = img.stride[1]
      .try_into()
      .map_err(|_| MediaError::Decode("vp9 U stride negative".to_string()))?;
    let v_stride: usize = img.stride[2]
      .try_into()
      .map_err(|_| MediaError::Decode("vp9 V stride negative".to_string()))?;
    let a_stride: usize = if has_alpha {
      img.stride[3]
        .try_into()
        .map_err(|_| MediaError::Decode("vp9 alpha stride negative".to_string()))?
    } else {
      0
    };

    let x_shift: usize = img
      .x_chroma_shift
      .try_into()
      .map_err(|_| MediaError::Decode("vp9 x_chroma_shift overflow".to_string()))?;
    let y_shift: usize = img
      .y_chroma_shift
      .try_into()
      .map_err(|_| MediaError::Decode("vp9 y_chroma_shift overflow".to_string()))?;
    if x_shift > 1 || y_shift > 1 {
      return Err(MediaError::Unsupported(format!(
        "vp9 chroma subsampling unsupported: x_chroma_shift={x_shift} y_chroma_shift={y_shift}"
      )));
    }

    let full_range = img.range == crate::VPX_CR_FULL_RANGE;
    let cs = img.cs;

    let mut rgba8 = vec![0u8; rgba_len];

    let chroma_width = (width + (1usize << x_shift) - 1) >> x_shift;
    let y_bytes_per_sample = if high_bit_depth { 2 } else { 1 };
    let chroma_bytes_per_sample = y_bytes_per_sample;
    let min_y_stride = width
      .checked_mul(y_bytes_per_sample)
      .ok_or_else(|| MediaError::Decode("vp9 Y stride overflow".to_string()))?;
    let min_uv_stride = chroma_width
      .checked_mul(chroma_bytes_per_sample)
      .ok_or_else(|| MediaError::Decode("vp9 UV stride overflow".to_string()))?;
    if y_stride < min_y_stride {
      return Err(MediaError::Decode(format!(
        "vp9 Y stride too small: stride={y_stride} min={min_y_stride}"
      )));
    }
    if u_stride < min_uv_stride || v_stride < min_uv_stride {
      return Err(MediaError::Decode(format!(
        "vp9 UV stride too small: u_stride={u_stride} v_stride={v_stride} min={min_uv_stride}"
      )));
    }
    if has_alpha && a_stride < min_y_stride {
      return Err(MediaError::Decode(format!(
        "vp9 alpha stride too small: stride={a_stride} min={min_y_stride}"
      )));
    }
    if high_bit_depth
      && (y_stride % 2 != 0
        || u_stride % 2 != 0
        || v_stride % 2 != 0
        || (has_alpha && a_stride % 2 != 0))
    {
      return Err(MediaError::Decode(format!(
        "vp9 high bit depth frame has odd stride: y_stride={y_stride} u_stride={u_stride} v_stride={v_stride} a_stride={a_stride}"
      )));
    }
    if high_bit_depth {
      // High bit depth frames use 16-bit samples packed into a byte buffer (little-endian on all
      // supported targets). Strides are in bytes.
      if bit_depth < 8 || bit_depth > 16 {
        return Err(MediaError::Unsupported(format!(
          "vp9 bit_depth unsupported: bit_depth={bit_depth} fmt=0x{fmt:x}"
        )));
      }
      let shift = bit_depth - 8;
      for row in 0..height {
        let y_row = unsafe { y_plane.add(row * y_stride) };
        let u_row = unsafe { u_plane.add((row >> y_shift) * u_stride) };
        let v_row = unsafe { v_plane.add((row >> y_shift) * v_stride) };
        let a_row = if has_alpha {
          Some(unsafe { a_plane.add(row * a_stride) })
        } else {
          None
        };

        for col in 0..width {
          let y16 = unsafe { ptr::read_unaligned(y_row.add(col * 2).cast::<u16>()) };
          let u16 = unsafe { ptr::read_unaligned(u_row.add((col >> x_shift) * 2).cast::<u16>()) };
          let v16 = unsafe { ptr::read_unaligned(v_row.add((col >> x_shift) * 2).cast::<u16>()) };
          let a = if let Some(a_row) = a_row {
            let a16 = unsafe { ptr::read_unaligned(a_row.add(col * 2).cast::<u16>()) };
            downshift_to_u8(a16, shift)
          } else {
            0xFF
          };

          let y = downshift_to_u8(y16, shift) as i32;
          let u = downshift_to_u8(u16, shift) as i32;
          let v = downshift_to_u8(v16, shift) as i32;

          let (r, g, b) = yuv_to_rgb(y, u, v, full_range, cs);
          let dst = (row * width + col) * 4;
          rgba8[dst] = clamp_u8(r);
          rgba8[dst + 1] = clamp_u8(g);
          rgba8[dst + 2] = clamp_u8(b);
          rgba8[dst + 3] = a;
        }
      }
    } else {
      for row in 0..height {
        let y_row = unsafe { y_plane.add(row * y_stride) };
        let u_row = unsafe { u_plane.add((row >> y_shift) * u_stride) };
        let v_row = unsafe { v_plane.add((row >> y_shift) * v_stride) };
        let a_row = if has_alpha {
          Some(unsafe { a_plane.add(row * a_stride) })
        } else {
          None
        };

        for col in 0..width {
          let y = unsafe { *y_row.add(col) } as i32;
          let u = unsafe { *u_row.add(col >> x_shift) } as i32;
          let v = unsafe { *v_row.add(col >> x_shift) } as i32;
          let a = if let Some(a_row) = a_row {
            unsafe { *a_row.add(col) }
          } else {
            0xFF
          };

          let (r, g, b) = yuv_to_rgb(y, u, v, full_range, cs);
          let dst = (row * width + col) * 4;
          rgba8[dst] = clamp_u8(r);
          rgba8[dst + 1] = clamp_u8(g);
          rgba8[dst + 2] = clamp_u8(b);
          rgba8[dst + 3] = a;
        }
      }
    }

    Ok(Vp9Frame {
      width: width as u32,
      height: height as u32,
      render_width: if img.r_w != 0 { img.r_w } else { width as u32 },
      render_height: if img.r_h != 0 { img.r_h } else { height as u32 },
      rgba8,
    })
  }
}

impl Drop for Vp9Decoder {
  fn drop(&mut self) {
    if self.initialized {
      unsafe {
        let _ = crate::vpx_codec_destroy(&mut self.ctx);
      }
    }
  }
}

fn clamp_u8(v: i32) -> u8 {
  if v <= 0 {
    0
  } else if v >= 255 {
    255
  } else {
    v as u8
  }
}

fn downshift_to_u8(sample: u16, shift: u32) -> u8 {
  if shift == 0 {
    return sample.min(255) as u8;
  }
  // Rounding downshift: add half-LSB before shifting.
  let add = 1u32 << (shift - 1);
  let v = ((sample as u32 + add) >> shift).min(255);
  v as u8
}

fn yuv_to_rgb(
  y: i32,
  u: i32,
  v: i32,
  full_range: bool,
  cs: crate::vpx_color_space_t,
) -> (i32, i32, i32) {
  let cs = match cs {
    // Treat both BT.601 and SMPTE.170 as BT.601.
    crate::VPX_CS_BT_601 | crate::VPX_CS_SMPTE_170 => crate::VPX_CS_BT_601,
    // SMPTE.240 is closer to BT.709 than BT.601 for our purposes.
    crate::VPX_CS_SMPTE_240 => crate::VPX_CS_BT_709,
    // VPX_CS_SRGB is typically used for RGB frames, but if we ever see it alongside YUV planes,
    // using BT.709 coefficients is a reasonable approximation.
    crate::VPX_CS_SRGB => crate::VPX_CS_BT_709,
    other => other,
  };

  if full_range {
    // Full-range YUV -> RGB using fixed-point math (16.16).
    let (r_coef, g_u_coef, g_v_coef, b_coef) = match cs {
      crate::VPX_CS_BT_709 => (103206, 12276, 30679, 121609),
      crate::VPX_CS_BT_2020 => (96639, 10784, 37444, 123299),
      // Default: BT.601.
      _ => (91881, 22554, 46802, 116130),
    };

    let d = u - 128;
    let e = v - 128;
    let r = y + ((r_coef * e + 32768) >> 16);
    let g = y - ((g_u_coef * d + g_v_coef * e + 32768) >> 16);
    let b = y + ((b_coef * d + 32768) >> 16);
    (r, g, b)
  } else {
    // Studio-range YUV -> RGB (16..235, 16..240).
    let (r_coef, g_u_coef, g_v_coef, b_coef) = match cs {
      crate::VPX_CS_BT_709 => (459, 55, 136, 541),
      crate::VPX_CS_BT_2020 => (430, 48, 166, 548),
      // Default: BT.601.
      _ => (409, 100, 208, 516),
    };

    let c = y - 16;
    let d = u - 128;
    let e = v - 128;
    let r = (298 * c + r_coef * e + 128) >> 8;
    let g = (298 * c - g_u_coef * d - g_v_coef * e + 128) >> 8;
    let b = (298 * c + b_coef * d + 128) >> 8;
    (r, g, b)
  }
}

fn codec_error_string(
  ctx: &mut crate::vpx_codec_ctx_t,
  code: Option<crate::vpx_codec_err_t>,
) -> String {
  unsafe {
    let mut parts = Vec::new();

    if let Some(code) = code {
      let code_ptr = crate::vpx_codec_err_to_string(code);
      if !code_ptr.is_null() {
        parts.push(CStr::from_ptr(code_ptr).to_string_lossy().into_owned());
      }
    }

    let err_ptr = crate::vpx_codec_error(ctx);
    if !err_ptr.is_null() {
      let msg = CStr::from_ptr(err_ptr).to_string_lossy().into_owned();
      if !msg.is_empty() {
        parts.push(msg);
      }
    }

    let detail_ptr = crate::vpx_codec_error_detail(ctx);
    if !detail_ptr.is_null() {
      let msg = CStr::from_ptr(detail_ptr).to_string_lossy().into_owned();
      if !msg.is_empty() {
        parts.push(msg);
      }
    }

    if parts.is_empty() {
      "unknown libvpx error".to_string()
    } else {
      parts.join(": ")
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rgba_from_image_rejects_large_dimensions_before_plane_validation() {
    let mut img = crate::vpx_image_t::default();
    img.fmt = crate::VPX_IMG_FMT_I420;
    img.bit_depth = 8;
    img.d_w = MAX_VIDEO_DIMENSION.saturating_add(1);
    img.d_h = 1;

    // Planes are null in the default image. The dimension cap check must fire before we validate
    // plane pointers so this test stays deterministic without allocating backing buffers.
    let err = Vp9Decoder::rgba_from_image(&img).unwrap_err();
    match err {
      MediaError::Unsupported(msg) => {
        assert!(
          msg.contains("dimensions") && msg.contains(&format!("{}", MAX_VIDEO_DIMENSION)),
          "unexpected message: {msg}"
        );
      }
      other => panic!("expected Unsupported due to cap, got {other:?}"),
    }
  }
}
