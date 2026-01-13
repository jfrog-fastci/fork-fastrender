use std::ffi::CStr;
use std::fmt;
use std::ptr;

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
  pub fn decode(&mut self, data: &[u8]) -> Result<Vec<Vp9Frame>, MediaError> {
    if !self.initialized {
      return Err(MediaError::Decode(
        "vp9 decoder not initialized".to_string(),
      ));
    }
    if data.is_empty() {
      return Ok(Vec::new());
    }

    let res = unsafe {
      crate::vpx_codec_decode(
        &mut self.ctx,
        data.as_ptr(),
        data
          .len()
          .try_into()
          .map_err(|_| MediaError::Decode("vp9 frame too large".to_string()))?,
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
  /// We currently **reject** non-8-bit output explicitly to avoid silent corruption from treating
  /// 16-bit planes as 8-bit.
  pub fn rgba_from_image(img: &crate::vpx_image_t) -> Result<Vp9Frame, MediaError> {
    let fmt = img.fmt as u32;
    let bit_depth = img.bit_depth;
    if bit_depth != 8 || (fmt & Self::VPX_IMG_FMT_HIGHBITDEPTH) != 0 {
      return Err(MediaError::Unsupported(format!(
        "vp9 bit_depth unsupported: bit_depth={bit_depth} fmt=0x{fmt:x}"
      )));
    }

    match fmt {
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

    let y_plane = img.planes[0];
    let u_plane = img.planes[1];
    let v_plane = img.planes[2];
    if y_plane.is_null() || u_plane.is_null() || v_plane.is_null() {
      return Err(MediaError::Decode(
        "vp9 frame has null Y/U/V plane pointers".to_string(),
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

    let mut rgba8 = vec![0u8; width * height * 4];
    for row in 0..height {
      let y_row = unsafe { y_plane.add(row * y_stride) };
      let u_row = unsafe { u_plane.add((row >> y_shift) * u_stride) };
      let v_row = unsafe { v_plane.add((row >> y_shift) * v_stride) };

      for col in 0..width {
        let y = unsafe { *y_row.add(col) } as i32;
        let u = unsafe { *u_row.add(col >> x_shift) } as i32;
        let v = unsafe { *v_row.add(col >> x_shift) } as i32;

        let (r, g, b) = if full_range {
          // Full-range BT.601.
          //
          // r = y + 1.402 * (v - 128)
          // g = y - 0.344136 * (u - 128) - 0.714136 * (v - 128)
          // b = y + 1.772 * (u - 128)
          //
          // Use fixed-point math for determinism/perf.
          let d = u - 128;
          let e = v - 128;
          let r = y + ((91881 * e + 32768) >> 16);
          let g = y - ((22554 * d + 46802 * e + 32768) >> 16);
          let b = y + ((116130 * d + 32768) >> 16);
          (r, g, b)
        } else {
          // Studio-range BT.601 (16..235, 16..240).
          let c = y - 16;
          let d = u - 128;
          let e = v - 128;
          let r = (298 * c + 409 * e + 128) >> 8;
          let g = (298 * c - 100 * d - 208 * e + 128) >> 8;
          let b = (298 * c + 516 * d + 128) >> 8;
          (r, g, b)
        };

        let dst = (row * width + col) * 4;
        rgba8[dst] = clamp_u8(r);
        rgba8[dst + 1] = clamp_u8(g);
        rgba8[dst + 2] = clamp_u8(b);
        rgba8[dst + 3] = 0xFF;
      }
    }

    Ok(Vp9Frame {
      width: width as u32,
      height: height as u32,
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
