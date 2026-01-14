//! Shared YUV conversion utilities.
//!
//! This crate exists so multiple codec backends (e.g. OpenH264 and libvpx) can share the exact same
//! pixel conversion logic without creating dependency cycles.

/// Convert a planar YUV 4:2:0 (a.k.a. YUV420p) frame to RGBA.
///
/// This utility is intended to be shared across video decoders that output
/// 8-bit planar YUV420 (e.g. H.264/openh264 and VP9/libvpx).
///
/// The format is:
/// - Y plane: full resolution (`width` x `height`)
/// - U plane: half resolution (`ceil(width/2)` x `ceil(height/2)`)
/// - V plane: half resolution (`ceil(width/2)` x `ceil(height/2)`)
///
/// `y_stride`, `u_stride`, and `v_stride` are in bytes.
///
/// The caller selects I420 vs YV12 by passing `u_plane`/`v_plane` in the desired
/// order:
/// - I420: `(..., u_plane, ..., v_plane, ...)`
/// - YV12: `(..., v_plane, ..., u_plane, ...)`
///
/// Output pixels are written in RGBA byte order with alpha forced to 255.
pub fn yuv420p_to_rgba(
  width: usize,
  height: usize,
  y_plane: &[u8],
  y_stride: usize,
  u_plane: &[u8],
  u_stride: usize,
  v_plane: &[u8],
  v_stride: usize,
  out_rgba: &mut [u8],
) {
  // Fast-path: empty frame.
  if width == 0 || height == 0 {
    return;
  }

  // Compute `ceil(width/2)` / `ceil(height/2)` without overflowing on `usize::MAX`.
  let uv_width = (width / 2).saturating_add(width % 2);
  let uv_height = (height / 2).saturating_add(height % 2);

  if y_stride < width || u_stride < uv_width || v_stride < uv_width {
    debug_assert!(
      false,
      "invalid YUV strides for frame (w={width} h={height} y_stride={y_stride} u_stride={u_stride} v_stride={v_stride})"
    );
    return;
  }

  let Some(pixel_count) = width.checked_mul(height) else {
    debug_assert!(false, "width*height overflow in yuv420p_to_rgba");
    return;
  };
  let Some(out_len_needed) = pixel_count.checked_mul(4) else {
    debug_assert!(false, "width*height*4 overflow in yuv420p_to_rgba");
    return;
  };
  if out_rgba.len() < out_len_needed {
    debug_assert!(
      false,
      "out_rgba buffer too small: need {out_len_needed} bytes, got {}",
      out_rgba.len()
    );
    return;
  }

  let Some(y_len_needed) = (height - 1)
    .checked_mul(y_stride)
    .and_then(|v| v.checked_add(width))
  else {
    debug_assert!(false, "y plane length overflow in yuv420p_to_rgba");
    return;
  };
  if y_plane.len() < y_len_needed {
    debug_assert!(
      false,
      "y_plane buffer too small: need {y_len_needed} bytes, got {}",
      y_plane.len()
    );
    return;
  }

  let Some(u_len_needed) = (uv_height - 1)
    .checked_mul(u_stride)
    .and_then(|v| v.checked_add(uv_width))
  else {
    debug_assert!(false, "u plane length overflow in yuv420p_to_rgba");
    return;
  };
  if u_plane.len() < u_len_needed {
    debug_assert!(
      false,
      "u_plane buffer too small: need {u_len_needed} bytes, got {}",
      u_plane.len()
    );
    return;
  }

  let Some(v_len_needed) = (uv_height - 1)
    .checked_mul(v_stride)
    .and_then(|v| v.checked_add(uv_width))
  else {
    debug_assert!(false, "v plane length overflow in yuv420p_to_rgba");
    return;
  };
  if v_plane.len() < v_len_needed {
    debug_assert!(
      false,
      "v_plane buffer too small: need {v_len_needed} bytes, got {}",
      v_plane.len()
    );
    return;
  }

  #[inline]
  fn clamp_to_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
  }

  for y in 0..height {
    let y_row_base = y * y_stride;
    let uv_row_base_u = (y / 2) * u_stride;
    let uv_row_base_v = (y / 2) * v_stride;
    for x in 0..width {
      let yv = y_plane[y_row_base + x] as i32;
      let uv_col = x / 2;
      let uv = u_plane[uv_row_base_u + uv_col] as i32;
      let vv = v_plane[uv_row_base_v + uv_col] as i32;

      // ITU-R BT.601 (limited range) integer approximation.
      let c = (yv - 16).max(0);
      let d = uv - 128;
      let e = vv - 128;

      let r = (298 * c + 409 * e + 128) >> 8;
      let g = (298 * c - 100 * d - 208 * e + 128) >> 8;
      let b = (298 * c + 516 * d + 128) >> 8;

      let out_off = (y * width + x) * 4;
      out_rgba[out_off] = clamp_to_u8(r);
      out_rgba[out_off + 1] = clamp_to_u8(g);
      out_rgba[out_off + 2] = clamp_to_u8(b);
      out_rgba[out_off + 3] = 255;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::yuv420p_to_rgba;

  #[test]
  fn yuv420p_to_rgba_2x2_known_output() {
    let width = 2;
    let height = 2;

    // One 2x2 luma block maps to a single chroma sample for 4:2:0.
    // Use neutral chroma (U=128, V=128) so the output is grayscale and easy to verify.
    let y_plane = [16u8, 235, 81, 145];
    let u_plane = [128u8];
    let v_plane = [128u8];

    let mut out = vec![0u8; width * height * 4];
    yuv420p_to_rgba(
      width,
      height,
      &y_plane,
      width,
      &u_plane,
      1,
      &v_plane,
      1,
      &mut out,
    );

    let expected = [
      0u8, 0, 0, 255, // (0,0)
      255, 255, 255, 255, // (1,0)
      76, 76, 76, 255, // (0,1)
      150, 150, 150, 255, // (1,1)
    ];
    assert_eq!(&out[..], &expected[..]);
  }

  #[test]
  fn yuv420p_to_rgba_respects_strides_with_padding() {
    let width = 4usize;
    let height = 4usize;
    let uv_width = width / 2;
    let uv_height = height / 2;

    // Tight (no padding) reference planes.
    let y_tight: Vec<u8> = (0..(width * height)).map(|i| 40u8 + (i as u8)).collect();
    let u_tight: Vec<u8> = vec![90, 140, 200, 40]; // 2x2
    let v_tight: Vec<u8> = vec![200, 40, 90, 140]; // 2x2

    let mut out_tight = vec![0u8; width * height * 4];
    yuv420p_to_rgba(
      width,
      height,
      &y_tight,
      width,
      &u_tight,
      uv_width,
      &v_tight,
      uv_width,
      &mut out_tight,
    );

    // Padded planes.
    let y_stride = width + 3;
    let u_stride = uv_width + 2;
    let v_stride = uv_width + 1;

    let mut y_padded = vec![0xEEu8; y_stride * height];
    for row in 0..height {
      let src_off = row * width;
      let dst_off = row * y_stride;
      y_padded[dst_off..dst_off + width].copy_from_slice(&y_tight[src_off..src_off + width]);
    }

    let mut u_padded = vec![0xDDu8; u_stride * uv_height];
    let mut v_padded = vec![0xCCu8; v_stride * uv_height];
    for row in 0..uv_height {
      let src_off = row * uv_width;
      let dst_u_off = row * u_stride;
      let dst_v_off = row * v_stride;
      u_padded[dst_u_off..dst_u_off + uv_width]
        .copy_from_slice(&u_tight[src_off..src_off + uv_width]);
      v_padded[dst_v_off..dst_v_off + uv_width]
        .copy_from_slice(&v_tight[src_off..src_off + uv_width]);
    }

    let mut out_padded = vec![0u8; width * height * 4];
    yuv420p_to_rgba(
      width,
      height,
      &y_padded,
      y_stride,
      &u_padded,
      u_stride,
      &v_padded,
      v_stride,
      &mut out_padded,
    );

    assert_eq!(out_padded, out_tight);
  }
}
