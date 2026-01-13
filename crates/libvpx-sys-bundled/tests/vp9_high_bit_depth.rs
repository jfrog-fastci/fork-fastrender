use libvpx_sys_bundled::{vpx_image_t, MediaError, Vp9Decoder, VPX_IMG_FMT_I42016};

#[test]
fn vp9_high_bit_depth_frames_are_rejected_explicitly() {
  // Constructing a real 10-bit VP9 bitstream is out of scope for a unit test; we only need to
  // ensure we never silently interpret high bit depth output as 8-bit.
  //
  // `Vp9Decoder::rgba_from_image` must inspect `vpx_image_t.bit_depth` and fail *before*
  // dereferencing any plane pointers.
  let mut img: vpx_image_t = unsafe { std::mem::zeroed() };
  img.fmt = VPX_IMG_FMT_I42016;
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
