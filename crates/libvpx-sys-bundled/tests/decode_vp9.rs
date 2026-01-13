use matroska_demuxer::{Frame, MatroskaFile, TrackType};
use std::ffi::CStr;
use std::io::Cursor;
use std::path::PathBuf;

#[test]
fn decodes_vp9_frame_from_webm_fixture() {
    // Reuse the workspace's deterministic CC0 fixture.
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/media/test_vp9_opus.webm");
    let bytes = std::fs::read(&fixture).expect("read test_vp9_opus.webm fixture");

    let mut mkv = MatroskaFile::open(Cursor::new(bytes)).expect("open Matroska/WebM");
    let video_track = mkv
        .tracks()
        .iter()
        .find(|t| t.track_type() == TrackType::Video && t.codec_id() == "V_VP9")
        .map(|t| t.track_number().get())
        .expect("VP9 track not found in fixture");

    let mut frame = Frame::default();
    loop {
        let has_frame = mkv.next_frame(&mut frame).expect("read Matroska frame");
        assert!(has_frame, "fixture contained no frames");
        if frame.track == video_track {
            break;
        }
    }

    let iface = unsafe { libvpx_sys_bundled::vpx_codec_vp9_dx() };
    assert!(!iface.is_null(), "vpx_codec_vp9_dx returned NULL");

    let cfg = libvpx_sys_bundled::vpx_codec_dec_cfg_t {
        threads: 1,
        w: 0,
        h: 0,
    };

    let mut codec = CodecCtx::new();
    codec.init(iface, &cfg);

    let mut si = libvpx_sys_bundled::vpx_codec_stream_info_t {
        sz: std::mem::size_of::<libvpx_sys_bundled::vpx_codec_stream_info_t>()
            .try_into()
            .unwrap(),
        ..Default::default()
    };
    let peek_err = unsafe {
        libvpx_sys_bundled::vpx_codec_peek_stream_info(
            iface,
            frame.data.as_ptr(),
            frame.data.len().try_into().unwrap(),
            &mut si,
        )
    };
    assert_eq!(
        peek_err,
        libvpx_sys_bundled::VPX_CODEC_OK,
        "vpx_codec_peek_stream_info failed: {}",
        err_to_string(peek_err)
    );
    assert_eq!((si.w, si.h), (64, 64), "unexpected stream dimensions");

    let err = unsafe {
        libvpx_sys_bundled::vpx_codec_decode(
            &mut codec.ctx,
            frame.data.as_ptr(),
            frame.data.len().try_into().expect("frame too large"),
            std::ptr::null_mut(),
            0,
        )
    };
    assert_eq!(
        err,
        libvpx_sys_bundled::VPX_CODEC_OK,
        "vpx_codec_decode failed: {}",
        codec_error_string(&mut codec.ctx, err)
    );

    let mut iter: libvpx_sys_bundled::vpx_codec_iter_t = std::ptr::null();
    let img = unsafe { libvpx_sys_bundled::vpx_codec_get_frame(&mut codec.ctx, &mut iter) };
    assert!(
        !img.is_null(),
        "vpx_codec_get_frame returned NULL after decoding a VP9 frame"
    );

    let (dw, dh) = unsafe { ((*img).d_w, (*img).d_h) };
    assert_eq!((dw, dh), (64, 64), "unexpected decoded frame dimensions");
}

fn err_to_string(err: libvpx_sys_bundled::vpx_codec_err_t) -> String {
    unsafe {
        let ptr = libvpx_sys_bundled::vpx_codec_err_to_string(err);
        if ptr.is_null() {
            "<null>".to_string()
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }
}

fn codec_error_string(
    ctx: &mut libvpx_sys_bundled::vpx_codec_ctx_t,
    err: libvpx_sys_bundled::vpx_codec_err_t,
) -> String {
    unsafe {
        let base = err_to_string(err);
        let detail_ptr = libvpx_sys_bundled::vpx_codec_error_detail(ctx);
        if detail_ptr.is_null() {
            base
        } else {
            format!(
                "{base}: {}",
                CStr::from_ptr(detail_ptr).to_string_lossy()
            )
        }
    }
}

struct CodecCtx {
    ctx: libvpx_sys_bundled::vpx_codec_ctx_t,
    initialized: bool,
}

impl CodecCtx {
    fn new() -> Self {
        Self {
            ctx: Default::default(),
            initialized: false,
        }
    }

    fn init(
        &mut self,
        iface: *mut libvpx_sys_bundled::vpx_codec_iface_t,
        cfg: &libvpx_sys_bundled::vpx_codec_dec_cfg_t,
    ) {
        let err = unsafe { libvpx_sys_bundled::vpx_codec_dec_init(&mut self.ctx, iface, cfg, 0) };
        assert_eq!(
            err,
            libvpx_sys_bundled::VPX_CODEC_OK,
            "vpx_codec_dec_init failed: {}",
            err_to_string(err)
        );
        self.initialized = true;
    }
}

impl Drop for CodecCtx {
    fn drop(&mut self) {
        if self.initialized {
            unsafe {
                libvpx_sys_bundled::vpx_codec_destroy(&mut self.ctx);
            }
        }
    }
}
