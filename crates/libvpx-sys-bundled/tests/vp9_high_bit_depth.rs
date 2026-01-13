use libvpx_sys_bundled::{
  vpx_image_t, MediaError, Vp9Decoder, VPX_CR_FULL_RANGE, VPX_IMG_FMT_I420, VPX_IMG_FMT_I42016,
};

#[test]
fn vp9_high_bit_depth_mismatch_is_rejected_explicitly() {
  // Constructing a real 10-bit VP9 bitstream is out of scope for a unit test; we only need to
  // ensure we never silently interpret high bit depth output as 8-bit.
  //
  // If `bit_depth != 8` but `VPX_IMG_FMT_HIGHBITDEPTH` is *not* set, `Vp9Decoder::rgba_from_image`
  // must bail out before dereferencing any plane pointers.
  let mut img: vpx_image_t = unsafe { std::mem::zeroed() };
  img.fmt = VPX_IMG_FMT_I420;
  img.bit_depth = 10;

  let err = Vp9Decoder::rgba_from_image(&img).expect_err("expected unsupported error");
  assert!(
    matches!(err, MediaError::Unsupported(_)),
    "unexpected error: {err}"
  );

  let msg = err.to_string();
  assert!(
    msg.contains("vp9 bit_depth"),
    "error should mention vp9 bit_depth, got: {msg}"
  );
  assert!(
    msg.contains("10"),
    "error should include bit depth, got: {msg}"
  );
}

#[test]
fn vp9_high_bit_depth_frames_are_downshifted_to_rgba8() {
  // Construct a minimal 2x2 I42016 (4:2:0) frame with 10-bit samples. Use full-range YUV with
  // neutral chroma so RGB should equal luma after downshifting to 8-bit.
  //
  // Layout (2x2):
  // Y plane: [0, 1023]
  //          [1023, 0]
  // U/V plane: single sample at 512 (center)
  let mut y = vec![0u16, 1023u16, 1023u16, 0u16];
  let mut u = vec![512u16];
  let mut v = vec![512u16];

  let mut img: vpx_image_t = unsafe { std::mem::zeroed() };
  img.fmt = VPX_IMG_FMT_I42016;
  img.bit_depth = 10;
  img.d_w = 2;
  img.d_h = 2;
  img.x_chroma_shift = 1;
  img.y_chroma_shift = 1;
  img.range = VPX_CR_FULL_RANGE;
  img.planes[0] = y.as_mut_ptr().cast::<u8>();
  img.planes[1] = u.as_mut_ptr().cast::<u8>();
  img.planes[2] = v.as_mut_ptr().cast::<u8>();
  img.stride[0] = 4; // 2 pixels * 2 bytes/sample
  img.stride[1] = 2; // 1 pixel * 2 bytes/sample
  img.stride[2] = 2;

  let frame = Vp9Decoder::rgba_from_image(&img).expect("expected successful downshift+convert");
  assert_eq!((frame.width, frame.height), (2, 2));
  assert_eq!(
    frame.rgba8,
    vec![
      0, 0, 0, 255, // row0 col0
      255, 255, 255, 255, // row0 col1
      255, 255, 255, 255, // row1 col0
      0, 0, 0, 255, // row1 col1
    ]
  );
}
