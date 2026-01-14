use crate::media::{MediaError, MediaPacket, MediaResult};

/// Decoded VP9 frame in RGBA8 format.
#[derive(Debug, Clone)]
pub struct DecodedVp9Frame {
  /// Presentation timestamp in nanoseconds.
  pub pts_ns: u64,
  pub width: u32,
  pub height: u32,
  pub rgba8: Vec<u8>,
}

/// VP9 bitstream decoder backed by libvpx.
pub struct Vp9Decoder {
  inner: libvpx_sys_bundled::Vp9Decoder,
}

// SAFETY: The underlying libvpx decoder context does not contain any Rust references and can be
// safely moved to another thread as long as it is not accessed concurrently. We only expose decode
// through `&mut self`, so callers cannot use it from multiple threads at once.
unsafe impl Send for Vp9Decoder {}

impl Vp9Decoder {
  /// Create a new VP9 decoder instance.
  pub fn new(threads: u32) -> MediaResult<Self> {
    let inner = libvpx_sys_bundled::Vp9Decoder::new(threads).map_err(map_libvpx_error)?;
    Ok(Self { inner })
  }

  /// Decode a compressed VP9 packet.
  ///
  /// Note: a single packet may yield 0+ output frames. For now, all output frames inherit the input
  /// packet's PTS.
  pub fn decode(&mut self, packet: &MediaPacket) -> MediaResult<Vec<DecodedVp9Frame>> {
    let frames = self
      .inner
      .decode(packet.as_slice())
      .map_err(map_libvpx_error)?;
    Ok(
      frames
        .into_iter()
        .map(|f| DecodedVp9Frame {
          pts_ns: packet.pts_ns,
          width: f.width,
          height: f.height,
          rgba8: f.rgba8,
        })
        .collect(),
    )
  }
}

fn map_libvpx_error(err: libvpx_sys_bundled::MediaError) -> MediaError {
  match err {
    // `MediaError::Unsupported` uses a `&'static str`, so preserve details in the Decode string for
    // now.
    libvpx_sys_bundled::MediaError::Unsupported(msg) => {
      MediaError::Decode(format!("unsupported VP9 stream: {msg}"))
    }
    libvpx_sys_bundled::MediaError::Decode(msg) => MediaError::Decode(msg),
  }
}
