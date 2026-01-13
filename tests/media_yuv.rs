use fastrender::media::yuv::yuv420p_to_rgba;

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

  // Expected output uses the BT.601 limited-range integer conversion:
  // - Y=16 -> 0
  // - Y=235 -> 255
  // - Y=81 -> 76
  // - Y=145 -> 150
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
    u_padded[dst_u_off..dst_u_off + uv_width].copy_from_slice(&u_tight[src_off..src_off + uv_width]);
    v_padded[dst_v_off..dst_v_off + uv_width].copy_from_slice(&v_tight[src_off..src_off + uv_width]);
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

