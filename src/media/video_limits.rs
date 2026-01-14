//! Hard limits for decoded video frames from untrusted media.
//!
//! Media containers and decoders should be treated as untrusted: corrupted or adversarial inputs
//! can claim absurd frame dimensions that would otherwise lead to unbounded allocations (OOM/DoS).
//!
//! These limits cap per-frame allocations in decoder backends. Higher layers (e.g. the
//! `SizeHintMediaFrameProvider`) may apply additional downscale/caching limits, but those are not a
//! substitute for limiting the *initial* decode buffer size.

/// Maximum width/height in pixels accepted from untrusted decoders.
///
/// This is a hard cap. Callers may apply a smaller cap (e.g. for caching), but should not allow
/// decoders to allocate frames larger than this.
pub const MAX_VIDEO_DIMENSION: u32 = 8192;

/// Maximum bytes allowed for a single decoded RGBA8 video frame.
///
/// This intentionally matches the hard cap used by the ffmpeg CLI backend.
pub const MAX_VIDEO_FRAME_BYTES: usize = 128 * 1024 * 1024;

