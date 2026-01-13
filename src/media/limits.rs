/// Limits controlling allocations and work during media demux/decode.
///
/// These limits are intended for hostile input (arbitrary web content) so a single media file
/// cannot trigger unbounded allocations or extremely long parsing/decoding work.
#[derive(Debug, Clone)]
pub struct MediaLimits {
  /// Maximum number of bytes the resource loader should accept for a media file.
  ///
  /// Loader-level enforcement lives outside the demux/decoder stack, but keeping the value here
  /// allows a single config source.
  pub max_media_bytes: usize,

  /// Maximum number of tracks (audio/video/subtitles) we will accept in a container.
  pub max_track_count: usize,

  /// Maximum number of samples per track we will accept when building sample tables / indexes.
  pub max_samples_per_track: usize,

  /// Maximum compressed packet/frame size (bytes) produced by demuxers.
  pub max_packet_bytes: usize,

  /// Maximum decoded video frame dimensions (width, height).
  pub max_video_dimensions: (u32, u32),

  /// Maximum bytes we will allocate for an RGBA frame (width * height * 4).
  ///
  /// This limit is checked independently from `max_video_dimensions` to provide an additional
  /// guardrail against large allocations even when dimensions appear nominally acceptable.
  pub max_rgba_bytes: usize,

  /// Maximum number of decoded audio samples produced by decoding a single packet.
  ///
  /// This is an upper bound on the length of the output buffer (i.e. total scalar samples across
  /// all channels) and is enforced before allocating output vectors.
  pub max_audio_samples_per_packet: usize,
}

impl Default for MediaLimits {
  fn default() -> Self {
    Self {
      // Keep consistent with the default `ResourcePolicy::max_response_bytes`.
      max_media_bytes: 50 * 1024 * 1024,
      // Typical files have a handful of tracks; allow a generous amount while remaining bounded.
      max_track_count: 8,
      // ~1h of 30fps video or ~70min of AAC (48kHz/1024) frames. Large enough for typical web
      // content while preventing multi-minute loops over gigantic sample tables or huge per-track
      // allocations.
      max_samples_per_track: 200_000,
      // Compressed frame/packet sizes above a few MB are pathological for web content.
      max_packet_bytes: 8 * 1024 * 1024,
      // Allow up to 8K dimensions but cap allocations via `max_rgba_bytes`.
      max_video_dimensions: (8192, 8192),
      // Enough for a 4K RGBA frame (~32MiB) with headroom, but prevents 8K RGBA allocations.
      max_rgba_bytes: 64 * 1024 * 1024,
      // Opus packets are typically <= 5760 samples/channel; keep generous headroom while bounded.
      max_audio_samples_per_packet: 100_000,
    }
  }
}
