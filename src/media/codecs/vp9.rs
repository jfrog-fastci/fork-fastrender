use crate::media::{MediaError, MediaLimits, MediaPacket, MediaResult};

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
  limits: MediaLimits,
}

// SAFETY: The underlying libvpx decoder context does not contain any Rust references and can be
// safely moved to another thread as long as it is not accessed concurrently. We only expose decode
// through `&mut self`, so callers cannot use it from multiple threads at once.
unsafe impl Send for Vp9Decoder {}

impl Vp9Decoder {
  /// Create a new VP9 decoder instance.
  pub fn new(threads: u32) -> MediaResult<Self> {
    Self::new_with_limits(threads, MediaLimits::default())
  }

  pub fn new_with_limits(threads: u32, limits: MediaLimits) -> MediaResult<Self> {
    let inner = libvpx_sys_bundled::Vp9Decoder::new(threads).map_err(map_libvpx_error)?;
    Ok(Self { inner, limits })
  }

  /// Decode a compressed VP9 packet.
  ///
  /// Note: a single packet may yield 0+ output frames (VP9 "superframes"). When this happens, the
  /// container typically provides a duration for the packet; we distribute PTS across the decoded
  /// frames to keep timestamps monotonic.
  pub fn decode(&mut self, packet: &MediaPacket) -> MediaResult<Vec<DecodedVp9Frame>> {
    if packet.data.len() > self.limits.max_packet_bytes {
      return Err(MediaError::resource_too_large(format!(
        "vp9 packet size {} exceeds max_packet_bytes {}",
        packet.data.len(),
        self.limits.max_packet_bytes
      )));
    }

    let decode_limits = libvpx_sys_bundled::DecodeLimits {
      max_video_dimensions: self.limits.max_video_dimensions,
      max_rgba_bytes: self.limits.max_rgba_bytes,
    };
    let frames = self
      .inner
      .decode_with_limits(packet.as_slice(), &decode_limits)
      .map_err(map_libvpx_error)?;
    if frames.is_empty() {
      return Ok(Vec::new());
    }

    let count = frames.len() as u64;
    let step_ns = if packet.duration_ns != 0 {
      packet.duration_ns / count
    } else {
      0
    };

    Ok(
      frames
        .into_iter()
        .enumerate()
        .map(|(idx, f)| {
          let pts_ns = if idx == 0 {
            packet.pts_ns
          } else if step_ns != 0 {
            packet
              .pts_ns
              .saturating_add(step_ns.saturating_mul(idx as u64))
          } else {
            // Ensure monotonic timestamps even when the container doesn't provide per-packet
            // durations.
            packet.pts_ns.saturating_add(idx as u64)
          };

          DecodedVp9Frame {
            pts_ns,
            width: f.width,
            height: f.height,
            rgba8: f.rgba8,
          }
        })
        .collect(),
    )
  }
}

fn map_libvpx_error(err: libvpx_sys_bundled::MediaError) -> MediaError {
  match err {
    libvpx_sys_bundled::MediaError::ResourceTooLarge(msg) => MediaError::resource_too_large(msg),
    libvpx_sys_bundled::MediaError::Unsupported(msg) => MediaError::unsupported(msg),
    libvpx_sys_bundled::MediaError::Decode(msg) => MediaError::Decode(msg),
  }
}
