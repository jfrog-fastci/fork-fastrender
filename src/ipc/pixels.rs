use super::IpcError;

fn protocol_violation(msg: impl Into<String>) -> IpcError {
  IpcError::ProtocolViolation { msg: msg.into() }
}

/// Copy a packed RGBA pixel buffer into a destination buffer with per-row stride/padding.
///
/// `src` is expected to be tightly packed `RGBA8` (4 bytes per pixel) with no row padding, i.e.
/// `src.len() == width_px * height_px * 4`.
///
/// `dst` is expected to be a buffer containing `height_px` rows, each `dst_stride_bytes` long. Each
/// row’s first `width_px * 4` bytes are overwritten with `src` pixel data; any remaining padding
/// bytes in the row are filled with zeroes for determinism/debugging.
///
/// This function never panics. All size math is checked and any violation results in
/// [`IpcError::ProtocolViolation`].
pub fn copy_packed_rgba_into_strided(
  src: &[u8],
  width_px: u32,
  height_px: u32,
  dst: &mut [u8],
  dst_stride_bytes: u32,
) -> Result<(), IpcError> {
  let bytes_per_px: u32 = 4;

  let row_bytes_u32 = width_px
    .checked_mul(bytes_per_px)
    .ok_or_else(|| protocol_violation("arithmetic overflow while computing row_bytes"))?;

  if dst_stride_bytes < row_bytes_u32 {
    return Err(IpcError::ProtocolViolation {
      msg: format!(
        "dst_stride_bytes < width_px*4: dst_stride_bytes={dst_stride_bytes} row_bytes={row_bytes_u32}"
      ),
    });
  }

  let expected_src_len_u64 = u64::from(row_bytes_u32)
    .checked_mul(u64::from(height_px))
    .ok_or_else(|| protocol_violation("arithmetic overflow while computing expected_src_len"))?;
  let expected_src_len: usize = usize::try_from(expected_src_len_u64)
    .map_err(|_| protocol_violation("expected_src_len does not fit in usize"))?;

  if src.len() != expected_src_len {
    return Err(IpcError::ProtocolViolation {
      msg: format!(
        "src length mismatch: expected {expected_src_len} bytes for {width_px}x{height_px} RGBA, got {}",
        src.len()
      ),
    });
  }

  let required_dst_len_u64 = u64::from(dst_stride_bytes)
    .checked_mul(u64::from(height_px))
    .ok_or_else(|| protocol_violation("arithmetic overflow while computing required_dst_len"))?;
  let required_dst_len: usize = usize::try_from(required_dst_len_u64)
    .map_err(|_| protocol_violation("required_dst_len does not fit in usize"))?;

  if dst.len() < required_dst_len {
    return Err(IpcError::ProtocolViolation {
      msg: format!(
        "dst buffer too small: need at least {required_dst_len} bytes for {height_px} rows of stride {dst_stride_bytes}, got {}",
        dst.len()
      ),
    });
  }

  let row_bytes: usize =
    usize::try_from(row_bytes_u32).map_err(|_| protocol_violation("row_bytes does not fit in usize"))?;
  let dst_stride: usize = usize::try_from(dst_stride_bytes)
    .map_err(|_| protocol_violation("dst_stride_bytes does not fit in usize"))?;
  let height: usize =
    usize::try_from(height_px).map_err(|_| protocol_violation("height_px does not fit in usize"))?;

  let mut src_offset: usize = 0;
  let mut dst_offset: usize = 0;

  for _ in 0..height {
    let src_end = src_offset
      .checked_add(row_bytes)
      .ok_or_else(|| protocol_violation("arithmetic overflow while computing src_end"))?;
    let dst_end = dst_offset
      .checked_add(dst_stride)
      .ok_or_else(|| protocol_violation("arithmetic overflow while computing dst_end"))?;

    let src_row = src
      .get(src_offset..src_end)
      .ok_or_else(|| IpcError::ProtocolViolation {
        msg: "src slice out of bounds".into(),
      })?;
    let dst_row = dst
      .get_mut(dst_offset..dst_end)
      .ok_or_else(|| IpcError::ProtocolViolation {
        msg: "dst slice out of bounds".into(),
      })?;

    // Copy pixels.
    dst_row[..row_bytes].copy_from_slice(src_row);

    // Zero padding.
    dst_row[row_bytes..].fill(0);

    src_offset = src_end;
    dst_offset = dst_end;
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn copies_3x2_with_padding_and_zeros_padding() {
    let width_px = 3;
    let height_px = 2;
    let src: Vec<u8> = (0u8..24u8).collect();

    let mut dst = vec![0xAAu8; 16 * 2];
    copy_packed_rgba_into_strided(&src, width_px, height_px, &mut dst, 16).unwrap();

    assert_eq!(&dst[0..12], &src[0..12]);
    assert_eq!(&dst[12..16], &[0, 0, 0, 0]);

    assert_eq!(&dst[16..28], &src[12..24]);
    assert_eq!(&dst[28..32], &[0, 0, 0, 0]);
  }

  #[test]
  fn rejects_too_small_dst_buffer() {
    let width_px = 3;
    let height_px = 2;
    let src: Vec<u8> = (0u8..24u8).collect();

    let mut dst = vec![0u8; 31]; // need 32 for stride 16 x height 2
    let err = copy_packed_rgba_into_strided(&src, width_px, height_px, &mut dst, 16).unwrap_err();
    assert!(matches!(err, IpcError::ProtocolViolation { .. }));
  }

  #[test]
  fn rejects_mismatched_src_length() {
    let width_px = 3;
    let height_px = 2;
    let src: Vec<u8> = (0u8..23u8).collect(); // should be 24

    let mut dst = vec![0u8; 16 * 2];
    let err = copy_packed_rgba_into_strided(&src, width_px, height_px, &mut dst, 16).unwrap_err();
    assert!(matches!(err, IpcError::ProtocolViolation { .. }));
  }
}
