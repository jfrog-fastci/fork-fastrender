use libvpx_sys_bundled::{vpx_image_t, DecodeLimits, MediaError, Vp9Decoder, VPX_IMG_FMT_I420};

#[test]
fn rejects_oversized_dimensions_before_dereferencing_planes() {
  let mut img: vpx_image_t = unsafe { std::mem::zeroed() };
  img.fmt = VPX_IMG_FMT_I420;
  img.bit_depth = 8;
  img.d_w = 2000;
  img.d_h = 2000;

  // Deliberately leave plane pointers null; the limit check should fire before any plane access.
  let limits = DecodeLimits {
    max_video_dimensions: (16, 16),
    max_rgba_bytes: 1024,
  };

  let err = Vp9Decoder::rgba_from_image_with_limits(&img, &limits).expect_err("expected error");
  assert!(
    matches!(err, MediaError::ResourceTooLarge(_)),
    "unexpected error: {err}"
  );
}

