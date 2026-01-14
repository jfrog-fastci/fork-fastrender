#![no_main]

use arbitrary::Arbitrary;
use fastrender::media::audio::PcmF32Queue;
use libfuzzer_sys::fuzz_target;
use std::time::Duration;

const MAX_OPS: usize = 256;
const MAX_FRAMES_PER_OP: usize = 8192;
const MAX_CHANNELS: usize = 8;
const MAX_BUFFERED_FRAMES: usize = 8192;

#[derive(Arbitrary, Debug)]
struct QueueInput {
  channels: u8,
  sample_rate_hz: u32,
  capacity_frames: u16,
  ops: Vec<QueueOp>,
}

#[derive(Arbitrary, Debug)]
struct QueueOp {
  kind: u8,
  frames: u16,
  start_pts_ns: u64,
  gain_bits: u32,
  data: Vec<f32>,
  seed: Vec<f32>,
}

fn bounded_channels(channels: u8) -> usize {
  (channels as usize % MAX_CHANNELS) + 1
}

fn bounded_sample_rate(sample_rate_hz: u32) -> u32 {
  (sample_rate_hz % 192_000).saturating_add(1)
}

fn sanitize_gain(bits: u32) -> f32 {
  let gain = f32::from_bits(bits);
  if gain.is_finite() {
    // Gains outside a reasonable range can cause extreme float values; clamp.
    gain.clamp(-4.0, 4.0)
  } else {
    0.0
  }
}

fn truncate_interleaved(mut samples: Vec<f32>, channels: usize, max_frames: usize) -> Vec<f32> {
  let max_samples = max_frames.saturating_mul(channels);
  if samples.len() > max_samples {
    samples.truncate(max_samples);
  }
  let usable = samples.len() - (samples.len() % channels);
  samples.truncate(usable);
  samples
}

fuzz_target!(|input: QueueInput| {
  let channels = bounded_channels(input.channels);
  let sample_rate_hz = bounded_sample_rate(input.sample_rate_hz);
  let capacity_frames = ((input.capacity_frames as usize) % MAX_BUFFERED_FRAMES).max(1);

  let queue = match PcmF32Queue::new(channels, sample_rate_hz, capacity_frames) {
    Ok(q) => q,
    Err(_) => return,
  };

  for op in input.ops.into_iter().take(MAX_OPS) {
    let frames = (op.frames as usize).min(MAX_FRAMES_PER_OP);
    let len_samples = frames.saturating_mul(channels);
    let start_pts_ns = op.start_pts_ns % 10_000_000_000; // 10s cap
    let start_pts = Duration::from_nanos(start_pts_ns);

    match op.kind % 6 {
      0 => {
        // Push with an explicit PTS base.
        let data = truncate_interleaved(op.data, channels, frames);
        let accepted = queue.push(&data, start_pts);
        debug_assert!(accepted <= data.len());
      }
      1 => {
        // Push without updating the PTS base.
        let data = truncate_interleaved(op.data, channels, frames);
        let accepted = queue.push_without_pts(&data);
        debug_assert!(accepted <= data.len());
      }
      2 => {
        // Pop into a fresh buffer.
        let mut out = vec![0.0f32; len_samples];
        let seed = truncate_interleaved(op.seed, channels, frames);
        let copy_len = seed.len().min(out.len());
        out[..copy_len].copy_from_slice(&seed[..copy_len]);
        let popped = queue.pop_into(&mut out);
        debug_assert!(popped <= out.len());
      }
      3 => {
        // Pop and mix-add into an existing buffer.
        let mut out = vec![0.0f32; len_samples];
        let seed = truncate_interleaved(op.seed, channels, frames);
        let copy_len = seed.len().min(out.len());
        out[..copy_len].copy_from_slice(&seed[..copy_len]);
        let gain = sanitize_gain(op.gain_bits);
        let popped = queue.pop_add_into(&mut out, gain);
        debug_assert!(popped <= out.len());
      }
      4 => {
        // Query metadata paths.
        let _ = queue.buffered_frames();
        let _ = queue.buffered_duration();
        let _ = queue.head_pts();
        let _ = queue.capacity_frames();
      }
      _ => {
        // Stress partial-frame truncation paths by not forcing output `frames`.
        let mut data = op.data;
        let max_samples = MAX_FRAMES_PER_OP.saturating_mul(channels);
        if data.len() > max_samples {
          data.truncate(max_samples);
        }
        let _ = queue.push(&data, start_pts);
      }
    }

    debug_assert!(queue.buffered_frames() <= queue.capacity_frames());
  }
});
