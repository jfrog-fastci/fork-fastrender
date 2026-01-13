use std::time::Duration;

/// Hard maximum accepted sample rate from untrusted containers/decoders.
pub const MAX_SAMPLE_RATE_HZ: u32 = 384_000;

/// Hard maximum accepted channel count from untrusted containers/decoders.
pub const MAX_CHANNELS: u16 = 8;

/// Hard maximum number of frames accepted per `push_audio` call.
///
/// This caps per-call allocations in resampling/conversion and provides a first line of defense
/// against decoders that report absurd timestamps/durations.
pub const MAX_FRAMES_PER_PUSH: usize = 131_072;

/// Hard maximum buffered audio duration allowed by configuration.
///
/// Buffering beyond this can cause unbounded memory usage (especially for high sample rates and
/// channel counts). Callers may set a lower limit, but never higher.
pub const MAX_BUFFERED_DURATION: Duration = Duration::from_secs(5);

