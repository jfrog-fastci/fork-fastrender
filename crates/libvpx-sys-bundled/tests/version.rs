use std::ffi::CStr;

#[test]
fn vpx_codec_version_str_is_non_null_c_string() {
    let ptr = unsafe { libvpx_sys_bundled::vpx_codec_version_str() };
    assert!(!ptr.is_null(), "vpx_codec_version_str returned NULL");

    let s = unsafe { CStr::from_ptr(ptr) };
    assert!(
        !s.to_bytes().is_empty(),
        "vpx_codec_version_str returned an empty string"
    );

    // Ensure the build actually includes the VP9 decoder interface (this crate exists primarily
    // for VP9 decode support).
    let vp9 = unsafe { libvpx_sys_bundled::vpx_codec_vp9_dx() };
    assert!(!vp9.is_null(), "vpx_codec_vp9_dx returned NULL (VP9 decoder not built?)");
}
